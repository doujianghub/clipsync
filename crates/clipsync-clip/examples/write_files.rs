//! 手动验证：把命令行给出的路径写入剪贴板。
//! 用法：cargo run -p clipsync-clip --example write_files -- /path/a /path/b
fn main() -> anyhow::Result<()> {
    let paths: Vec<std::path::PathBuf> = std::env::args().skip(1).map(Into::into).collect();
    if paths.is_empty() {
        eprintln!("用法: write_files <路径>...");
        std::process::exit(2);
    }
    clipsync_clip::filelist::write_file_paths(&paths)?;
    println!("已写入 {} 个路径", paths.len());
    Ok(())
}
