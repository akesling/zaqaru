//! Native preparation/validation helper. Runtime compilation lives in the
//! portable optimizer Block, never in a native process launched by an Assembly.
#[allow(dead_code)] // checkpoint is also compiled into the portable optimizer.
mod artifact;
#[cfg(test)]
mod tests;
use anyhow::{Result, ensure};
use std::path::Path;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("bake") => {
            ensure!(
                args.len() >= 6,
                "bake ARCHIVE ROOTFS OUTPUT COMMAND [ARG...]"
            );
            let tree = image::tree::Tree::from_directory(Path::new(&args[3]))?;
            let command = args[5..]
                .iter()
                .map(|s| s.as_bytes().to_vec())
                .collect::<Vec<_>>();
            let image = image::bake_tree_with_command(&tree, &command)?;
            bake::link_with_exports(
                &image,
                &bake::Guest::from_archive(&args[2]),
                Path::new(&args[4]),
                &[
                    "zaqaru_freeze",
                    "zaqaru_retire",
                    "zaqaru_resume",
                    "zaqaru_compiled_retired",
                    "zaqaru_region_retired",
                    "zaqaru_region_entries",
                    "zaqaru_guard_windows",
                    "zaqaru_region_limit",
                    "weval.pending.head",
                    "weval.is.wevaled",
                    "weval.func.0",
                    "weval.func.1",
                ],
            )?;
        }
        Some("executable") => {
            ensure!(args.len() == 4, "executable INPUT OUTPUT");
            std::fs::write(&args[3], artifact::executable(&std::fs::read(&args[2])?)?)?;
        }
        _ => anyhow::bail!("expected bake or executable"),
    }
    Ok(())
}
