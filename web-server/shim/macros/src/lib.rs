//! `#[tauri::command]` 在网页服务里只是透明属性：命令函数由 HTTP 分发器直接调用。
use proc_macro::TokenStream;
#[proc_macro_attribute]
pub fn command(_attr: TokenStream, item: TokenStream) -> TokenStream { item }
