use std::{
    cell::RefCell,
    collections::{HashMap, HashSet, VecDeque},
    env, fmt,
    hash::{Hash, Hasher},
    io::{self, IsTerminal},
    rc::Rc,
    sync::{
        Arc,
        mpsc::{self, Receiver, Sender},
    },
};

use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    text::Line,
    widgets::{Paragraph, Widget, WidgetRef},
};
use ratatui_image::{
    picker::Picker,
    sliced::{SignedPosition, SlicedImage, SlicedProtocol},
};

const MAX_PREPARED_IMAGES: usize = 64;
const MAX_IN_FLIGHT_IMAGES: usize = 4;
const IMAGE_LOADING_PLACEHOLDER: &str = "[image loading]";

pub struct ImageView<'a> {
    renderer: &'a ImageRenderer,
    source: &'a ImageSource,
    size: Option<Size>,
    y_offset: i16,
}

impl<'a> ImageView<'a> {
    pub fn new(renderer: &'a ImageRenderer, source: &'a ImageSource) -> Self {
        Self {
            renderer,
            source,
            size: None,
            y_offset: 0,
        }
    }

    pub fn size(mut self, size: Size) -> Self {
        self.size = Some(size);
        self
    }

    pub fn y_offset(mut self, y_offset: i16) -> Self {
        self.y_offset = y_offset;
        self
    }
}

impl WidgetRef for ImageView<'_> {
    fn render_ref(&self, area: Rect, buf: &mut Buffer) {
        if area.is_empty() {
            return;
        }

        let size = self
            .size
            .unwrap_or_else(|| Size::new(area.width, area.height));
        let prepared = self.renderer.prepare_for_area(self.source, size);
        if prepared.render_buffer_at(area, self.y_offset, buf) {
            return;
        }

        if let Some(placeholder) = prepared.placeholder() {
            Paragraph::new(Line::from(placeholder.to_string())).render(area, buf);
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ImageSource {
    Url { url: String },
    Data { mime_type: String, data: Arc<[u8]> },
}

impl ImageSource {
    pub fn url(url: impl Into<String>) -> Self {
        Self::Url { url: url.into() }
    }

    pub fn data(mime_type: impl Into<String>, data: impl Into<Arc<[u8]>>) -> Self {
        Self::Data {
            mime_type: mime_type.into(),
            data: data.into(),
        }
    }

    pub fn placeholder_text(&self) -> String {
        match self {
            Self::Url { .. } => "[image: url]".to_string(),
            Self::Data { mime_type, data } => {
                format!("[image: {mime_type}, {} bytes]", data.len())
            }
        }
    }
}

#[derive(Clone)]
pub struct ImageRenderer {
    picker: Picker,
    inner: Rc<ImageRendererInner>,
}

struct ImageRendererInner {
    state: RefCell<ImageCacheState>,
    tx: Sender<PreparedImageResult>,
    rx: RefCell<Receiver<PreparedImageResult>>,
}

#[derive(Default)]
struct ImageCacheState {
    prepared: HashMap<PreparedImageKey, PreparedImage>,
    order: VecDeque<PreparedImageKey>,
    in_flight: HashSet<PreparedImageKey>,
}

impl ImageRenderer {
    pub fn new(picker: Picker) -> Self {
        let (tx, rx) = mpsc::channel();
        Self {
            picker,
            inner: Rc::new(ImageRendererInner {
                state: RefCell::new(ImageCacheState::default()),
                tx,
                rx: RefCell::new(rx),
            }),
        }
    }

    pub fn halfblocks() -> Self {
        Self::new(Picker::halfblocks())
    }

    pub fn from_terminal_query_or_halfblocks() -> Self {
        if !should_query_terminal() {
            return Self::halfblocks();
        }

        Self::new(Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks()))
    }

    fn prepare_for_area(&self, image: &ImageSource, size: Size) -> PreparedImage {
        self.drain_prepared();

        let key = PreparedImageKey::new(image, size);
        if let Some(prepared) = self.cached(&key) {
            return prepared;
        }

        let ImageSource::Data { mime_type, data } = image else {
            let prepared = PreparedImage::Placeholder(image.placeholder_text());
            self.finish_prepared(key, prepared.clone());
            return prepared;
        };

        if self.mark_in_flight(key.clone()) {
            let tx = self.inner.tx.clone();
            let picker = self.picker.clone();
            let mime_type = mime_type.clone();
            let data = data.clone();
            std::thread::spawn(move || {
                let prepared = prepare_data_image(&picker, &mime_type, &data, size);
                let _ = tx.send(PreparedImageResult { key, prepared });
            });
        }

        PreparedImage::Loading
    }

    pub fn drain_prepared(&self) -> bool {
        let mut drained = false;
        loop {
            let Ok(result) = self.inner.rx.borrow_mut().try_recv() else {
                break;
            };
            drained = true;
            self.finish_prepared(result.key, result.prepared);
        }
        drained
    }

    pub fn has_in_flight(&self) -> bool {
        !self.inner.state.borrow().in_flight.is_empty()
    }

    fn cached(&self, key: &PreparedImageKey) -> Option<PreparedImage> {
        let mut state = self.inner.state.borrow_mut();
        let prepared = state.prepared.get(key).cloned()?;
        state.touch(key.clone());
        Some(prepared)
    }

    fn mark_in_flight(&self, key: PreparedImageKey) -> bool {
        let mut state = self.inner.state.borrow_mut();
        if state.in_flight.len() >= MAX_IN_FLIGHT_IMAGES {
            return false;
        }
        state.in_flight.insert(key)
    }

    fn finish_prepared(&self, key: PreparedImageKey, prepared: PreparedImage) {
        let mut state = self.inner.state.borrow_mut();
        state.in_flight.remove(&key);
        state.prepared.insert(key.clone(), prepared);
        state.touch(key);
        state.evict_oldest();
    }

    #[cfg(test)]
    fn prepared_cache_len(&self) -> usize {
        self.drain_prepared();
        self.inner.state.borrow().prepared.len()
    }
}

impl ImageCacheState {
    fn touch(&mut self, key: PreparedImageKey) {
        self.order.retain(|existing| existing != &key);
        self.order.push_back(key);
    }

    fn evict_oldest(&mut self) {
        while self.prepared.len() > MAX_PREPARED_IMAGES {
            let Some(key) = self.order.pop_front() else {
                break;
            };
            self.prepared.remove(&key);
        }
    }
}

impl fmt::Debug for ImageRenderer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let cache_len = self
            .inner
            .state
            .try_borrow()
            .map(|state| state.prepared.len())
            .ok();
        f.debug_struct("ImageRenderer")
            .field("picker", &self.picker)
            .field("prepared_cache_len", &cache_len)
            .finish()
    }
}

fn prepare_data_image(picker: &Picker, mime_type: &str, data: &[u8], size: Size) -> PreparedImage {
    match ::image::load_from_memory(data).and_then(|image| {
        SlicedProtocol::new(picker, image, Some(size))
            .map_err(|error| ::image::ImageError::IoError(io::Error::other(error.to_string())))
    }) {
        Ok(protocol) => PreparedImage::Image {
            protocol: Arc::new(protocol),
        },
        Err(_) => PreparedImage::Placeholder(format!(
            "[image: invalid {mime_type}, {} bytes]",
            data.len()
        )),
    }
}

struct PreparedImageResult {
    key: PreparedImageKey,
    prepared: PreparedImage,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct PreparedImageKey {
    image: ImageFingerprint,
    width: u16,
    height: u16,
}

impl PreparedImageKey {
    fn new(image: &ImageSource, size: Size) -> Self {
        Self {
            image: ImageFingerprint::new(image),
            width: size.width,
            height: size.height,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ImageFingerprint {
    Url(String),
    Data(ImageDataIdentity),
}

#[derive(Debug, Clone)]
struct ImageDataIdentity {
    mime_type: String,
    data: Arc<[u8]>,
}

impl PartialEq for ImageDataIdentity {
    fn eq(&self, other: &Self) -> bool {
        self.mime_type == other.mime_type && Arc::ptr_eq(&self.data, &other.data)
    }
}

impl Eq for ImageDataIdentity {}

impl Hash for ImageDataIdentity {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.mime_type.hash(state);
        self.data.as_ptr().hash(state);
        self.data.len().hash(state);
    }
}

impl ImageFingerprint {
    fn new(image: &ImageSource) -> Self {
        match image {
            ImageSource::Url { url } => Self::Url(url.clone()),
            ImageSource::Data { mime_type, data } => Self::Data(ImageDataIdentity {
                mime_type: mime_type.clone(),
                data: data.clone(),
            }),
        }
    }
}

fn should_query_terminal() -> bool {
    let term = env::var("TERM").ok();
    should_query_terminal_for(
        term.as_deref(),
        io::stdin().is_terminal(),
        io::stdout().is_terminal(),
    )
}

fn should_query_terminal_for(
    term: Option<&str>,
    stdin_is_terminal: bool,
    stdout_is_terminal: bool,
) -> bool {
    let Some(term) = term else {
        return false;
    };

    !term.is_empty() && term != "dumb" && stdin_is_terminal && stdout_is_terminal
}

impl Default for ImageRenderer {
    fn default() -> Self {
        Self::halfblocks()
    }
}

#[derive(Clone)]
enum PreparedImage {
    Image { protocol: Arc<SlicedProtocol> },
    Placeholder(String),
    Loading,
}

impl PreparedImage {
    fn render_buffer_at(&self, area: Rect, y: i16, buf: &mut Buffer) -> bool {
        match self {
            Self::Image { protocol } => {
                SlicedImage::new(protocol, SignedPosition::from((0, y))).render(area, buf);
                true
            }
            Self::Placeholder(_) | Self::Loading => false,
        }
    }

    #[cfg(test)]
    fn is_image(&self) -> bool {
        matches!(self, Self::Image { .. })
    }

    fn placeholder(&self) -> Option<&str> {
        match self {
            Self::Placeholder(text) => Some(text),
            Self::Loading => Some(IMAGE_LOADING_PLACEHOLDER),
            Self::Image { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ::image::{DynamicImage, ImageFormat, RgbaImage};
    use ratatui::buffer::Buffer;
    use ratatui::widgets::WidgetRef;
    use std::{io::Cursor, time::Duration};

    #[test]
    fn terminal_query_is_skipped_for_non_interactive_terms() {
        assert!(!should_query_terminal_for(Some("dumb"), true, true));
        assert!(!should_query_terminal_for(None, true, true));
        assert!(!should_query_terminal_for(
            Some("xterm-ghostty"),
            false,
            true
        ));
        assert!(!should_query_terminal_for(
            Some("xterm-ghostty"),
            true,
            false
        ));
        assert!(should_query_terminal_for(Some("xterm-ghostty"), true, true));
    }

    #[test]
    fn valid_data_renders_sliced_image_to_buffer() {
        let renderer = ImageRenderer::halfblocks();
        let image = ImageSource::data("image/png", png_bytes());

        let loading = renderer.prepare_for_area(&image, Size::new(4, 4));
        assert_eq!(loading.placeholder(), Some(IMAGE_LOADING_PLACEHOLDER));
        wait_for_image_jobs(&renderer);

        let prepared = renderer.prepare_for_area(&image, Size::new(4, 4));
        let area = Rect::new(0, 0, 4, 4);
        let mut buffer = Buffer::empty(area);

        assert!(prepared.render_buffer_at(area, 0, &mut buffer));
        assert!(
            buffer
                .content()
                .iter()
                .any(|cell| matches!(cell.symbol(), "▀" | "▄"))
        );
    }

    #[test]
    fn prepared_data_image_is_reused_for_same_area() {
        let renderer = ImageRenderer::halfblocks();
        let image = ImageSource::data("image/png", png_bytes());

        assert_eq!(
            renderer
                .prepare_for_area(&image, Size::new(4, 4))
                .placeholder(),
            Some(IMAGE_LOADING_PLACEHOLDER)
        );
        wait_for_image_jobs(&renderer);
        assert!(
            renderer
                .prepare_for_area(&image, Size::new(4, 4))
                .is_image()
        );

        assert_eq!(renderer.prepared_cache_len(), 1);
    }

    #[test]
    fn prepared_cache_keeps_data_identity_alive() {
        let renderer = ImageRenderer::halfblocks();
        let data: Arc<[u8]> = png_bytes().into();
        let image = ImageSource::data("image/png", data.clone());

        assert_eq!(
            renderer
                .prepare_for_area(&image, Size::new(4, 4))
                .placeholder(),
            Some(IMAGE_LOADING_PLACEHOLDER)
        );
        wait_for_image_jobs(&renderer);
        assert!(
            renderer
                .prepare_for_area(&image, Size::new(4, 4))
                .is_image()
        );
        drop(image);

        assert!(Arc::strong_count(&data) > 1);
    }

    #[test]
    fn prepares_data_image_again_for_different_area() {
        let renderer = ImageRenderer::halfblocks();
        let image = ImageSource::data("image/png", png_bytes());

        assert_eq!(
            renderer
                .prepare_for_area(&image, Size::new(4, 4))
                .placeholder(),
            Some(IMAGE_LOADING_PLACEHOLDER)
        );
        wait_for_image_jobs(&renderer);
        assert_eq!(renderer.prepared_cache_len(), 1);

        assert_eq!(
            renderer
                .prepare_for_area(&image, Size::new(8, 4))
                .placeholder(),
            Some(IMAGE_LOADING_PLACEHOLDER)
        );
        wait_for_image_jobs(&renderer);
        assert!(
            renderer
                .prepare_for_area(&image, Size::new(8, 4))
                .is_image()
        );

        assert_eq!(renderer.prepared_cache_len(), 2);
    }

    #[test]
    fn invalid_data_falls_back_to_compact_placeholder() {
        let renderer = ImageRenderer::halfblocks();
        let image = ImageSource::data("image/png", vec![1, 2, 3]);

        assert_eq!(
            renderer
                .prepare_for_area(&image, Size::new(4, 4))
                .placeholder(),
            Some(IMAGE_LOADING_PLACEHOLDER)
        );
        wait_for_image_jobs(&renderer);

        let prepared = renderer.prepare_for_area(&image, Size::new(4, 4));

        assert_eq!(
            prepared.placeholder(),
            Some("[image: invalid image/png, 3 bytes]")
        );
    }

    #[test]
    fn url_image_remains_placeholder() {
        let image = ImageSource::url("https://example.com/a.png");

        let prepared = ImageRenderer::halfblocks().prepare_for_area(&image, Size::new(4, 4));

        assert_eq!(prepared.placeholder(), Some("[image: url]"));
    }

    #[test]
    fn image_view_renders_url_placeholder() {
        let renderer = ImageRenderer::halfblocks();
        let image = ImageSource::url("https://example.com/a.png");
        let area = Rect::new(0, 0, 16, 1);
        let mut buffer = Buffer::empty(area);

        ImageView::new(&renderer, &image).render_ref(area, &mut buffer);

        let text = (0..area.width)
            .map(|x| buffer[(x, 0)].symbol())
            .collect::<String>();
        assert!(text.contains("[image: url]"));
    }

    fn wait_for_image_jobs(renderer: &ImageRenderer) {
        for _ in 0..100 {
            renderer.drain_prepared();
            if !renderer.has_in_flight() {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        panic!("image prepare job did not finish");
    }

    fn png_bytes() -> Vec<u8> {
        let mut data = Vec::new();
        let image = DynamicImage::ImageRgba8(
            RgbaImage::from_vec(1, 1, vec![255, 0, 0, 255]).expect("valid rgba image"),
        );

        image
            .write_to(&mut Cursor::new(&mut data), ImageFormat::Png)
            .expect("valid png");

        data
    }
}
