fn main() -> anyhow::Result<()> {
    let destination = std::env::args_os()
        .nth(1)
        .ok_or_else(|| anyhow::anyhow!("usage: runtime_baseline OUTPUT.json"))?;
    let baseline = dial::benchmark::runtime::measure(256)?;
    let path = std::path::Path::new(&destination);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(&baseline)?)?;
    Ok(())
}
