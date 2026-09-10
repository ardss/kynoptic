// Windows 下把应用图标与版本信息嵌进 exe 资源段（文件浏览器/安装器显示的图标）。
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/kynoptic.ico");
        res.set("ProductName", "Kynoptic");
        res.set("FileDescription", "Kynoptic tray shell");
        if let Err(e) = res.compile() {
            panic!("winresource compile failed: {e}");
        }
    }
}
