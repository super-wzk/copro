use copro_api::message::{ImageContent, InputContent};

pub trait ToolOutput: Send {
    fn into_content(self) -> Result<Vec<InputContent>, String>;
}

impl ToolOutput for String {
    fn into_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self)])
    }
}

impl ToolOutput for &'static str {
    fn into_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self.to_string())])
    }
}

impl ToolOutput for InputContent {
    fn into_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![self])
    }
}

impl ToolOutput for Vec<InputContent> {
    fn into_content(self) -> Result<Vec<InputContent>, String> {
        Ok(self)
    }
}

impl ToolOutput for ImageContent {
    fn into_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Image(self)])
    }
}

impl ToolOutput for () {
    fn into_content(self) -> Result<Vec<InputContent>, String> {
        Ok(Vec::new())
    }
}

macro_rules! impl_text_tool_output {
    ($($ty:ty),* $(,)?) => {
        $(
            impl ToolOutput for $ty {
                fn into_content(self) -> Result<Vec<InputContent>, String> {
                    Ok(vec![InputContent::Text(self.to_string())])
                }
            }
        )*
    };
}

impl_text_tool_output!(bool, i8, i16, i32, i64, i128, isize);
impl_text_tool_output!(u8, u16, u32, u64, u128, usize);
impl_text_tool_output!(f32, f64);
