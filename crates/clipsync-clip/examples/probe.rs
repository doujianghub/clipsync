//! 手动验证平台探测：打印当前剪贴板的敏感标记、变化令牌、是否含图片、文件列表。
//! 用法：cargo run -p clipsync-clip --example probe
fn main() {
    println!(
        "sensitive   = {}",
        clipsync_clip::sensitive::clipboard_is_sensitive()
    );
    println!(
        "change_token= {:?}",
        clipsync_clip::change_token::clipboard_change_token()
    );
    println!(
        "has_image   = {:?}",
        clipsync_clip::formats::clipboard_has_image()
    );
    println!("files_supported = {}", clipsync_clip::filelist::supported());
    match clipsync_clip::filelist::read_file_paths() {
        Ok(Some(v)) => println!("file_paths  = {v:?}"),
        Ok(None) => println!("file_paths  = (none)"),
        Err(e) => println!("file_paths  = ERR {e:#}"),
    }
}
