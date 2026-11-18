fn main() -> anyhow::Result<()> {
    soju_fuzz::run(soju_fuzz::Target::Irc, "irc")
}
