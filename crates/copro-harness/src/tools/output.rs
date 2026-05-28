use copro_api::message::{ImageContent, InputContent};
use serde::Serialize;
use serde_json::Value;

pub trait ToolOutput: Send {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Json<T>(pub T);

impl<T> Json<T> {
    pub fn new(value: T) -> Self {
        Self(value)
    }

    pub fn into_inner(self) -> T {
        self.0
    }
}

impl<T> ToolOutput for Json<T>
where
    T: Serialize + Send,
{
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        json_output_content(self.0)
    }
}

impl ToolOutput for String {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self)])
    }
}

impl ToolOutput for &'static str {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self.to_string())])
    }
}

impl ToolOutput for InputContent {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![self])
    }
}

impl ToolOutput for Vec<InputContent> {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        Ok(self)
    }
}

impl ToolOutput for ImageContent {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Image(self)])
    }
}

impl ToolOutput for () {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        json_output_content(self)
    }
}

macro_rules! impl_json_tool_output {
    ($($ty:ty),* $(,)?) => {
        $(
            impl ToolOutput for $ty {
                fn into_tool_result_content(self) -> std::result::Result<Vec<InputContent>, String> {
                    json_output_content(self)
                }
            }
        )*
    };
}

impl_json_tool_output!(bool, i8, i16, i32, i64, i128, isize);
impl_json_tool_output!(u8, u16, u32, u64, u128, usize);
impl_json_tool_output!(f32, f64);

impl ToolOutput for Value {
    fn into_tool_result_content(self) -> Result<Vec<InputContent>, String> {
        Ok(vec![InputContent::Text(self.to_string())])
    }
}

fn json_output_content<T>(output: T) -> Result<Vec<InputContent>, String>
where
    T: Serialize,
{
    let value = serde_json::to_value(output).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(&value).unwrap_or_else(|_| format!("{value:?}"));
    Ok(vec![InputContent::Text(text)])
}
