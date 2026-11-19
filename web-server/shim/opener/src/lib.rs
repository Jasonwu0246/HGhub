//! 网页版不在服务器上打开外部程序；前端适配层直接 window.open。
pub struct Opener;
impl Opener {
    pub fn open_url<S: Into<String>>(&self, _url: &str, _with: Option<S>) -> Result<(), String> {
        Err("网页版不支持由服务器打开链接".into())
    }
}
pub trait OpenerExt { fn opener(&self) -> Opener; }
impl OpenerExt for tauri::AppHandle { fn opener(&self) -> Opener { Opener } }
