fn main() -> anyhow::Result<()> {
    let _ = vhrn_proxy::Config::resolve(|key| std::env::var(key).ok())?;
    Ok(())
}
