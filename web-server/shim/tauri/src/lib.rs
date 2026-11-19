//! 极小的 tauri 替身：让 src-tauri/src 里的核心模块原样编译进 HTTP 服务。
use std::path::PathBuf;
pub use http;
pub use tauri_shim_macros::command;

pub struct State<'a, T: 'a>(pub &'a T);
impl<'a, T> std::ops::Deref for State<'a, T> {
    type Target = T;
    fn deref(&self) -> &T { self.0 }
}

#[derive(Clone)]
pub struct AppHandle { pub data_dir: PathBuf }

pub struct PathResolver<'a>(&'a AppHandle);
impl PathResolver<'_> {
    pub fn app_data_dir(&self) -> Result<PathBuf, std::io::Error> { Ok(self.0.data_dir.clone()) }
}

pub trait Manager { fn path(&self) -> PathResolver<'_>; }
impl Manager for AppHandle { fn path(&self) -> PathResolver<'_> { PathResolver(self) } }
