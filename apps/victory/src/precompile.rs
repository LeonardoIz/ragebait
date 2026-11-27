//! `victory precompile`: the shader warm-up and the pipeline pre-build,
//! each done only when a stamp in the cache folder says what it was last
//! done for no longer holds. `victory run --execute` runs it first, as a
//! process of its own, with `VICTORY_AUTO_PRECOMPILE=1` (see `run`).

use std::path::{Path, PathBuf};

use anyhow::Result;

const STAMP: &str = "precompile.txt";

/// What the cache was last precompiled for.
#[derive(Debug, Default, PartialEq)]
struct Stamp {
    /// The translator, its settings and the host's adapter and driver
    /// (`victory_gpu::pipeline::precompile_key`).
    key: String,
    /// The folders warmed.
    paths: String,
    /// The size of the recorded pipeline descriptions pre-built (the
    /// file only grows, or starts again in a new format).
    recorded: u64,
}

impl Stamp {
    fn to_text(&self) -> String {
        format!("key={}\npaths={}\nrecorded={}\n", self.key, self.paths, self.recorded)
    }

    fn parse(text: &str) -> Option<Stamp> {
        let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k)?.strip_prefix('='));
        Some(Stamp {
            key: get("key")?.to_owned(),
            paths: get("paths")?.to_owned(),
            recorded: get("recorded")?.parse().ok()?,
        })
    }
}

/// Which steps `now` needs after `old`: (warm the shaders, pre-build the
/// pipelines). A pre-build with nothing recorded has nothing to do.
fn steps(old: Option<&Stamp>, now: &Stamp) -> (bool, bool) {
    let key = old.is_none_or(|o| o.key != now.key);
    let warm = key || old.is_some_and(|o| o.paths != now.paths);
    let prebuild = now.recorded > 0 && (key || old.is_some_and(|o| o.recorded != now.recorded));
    (warm, prebuild)
}

fn recorded(pipelines: &Path) -> u64 {
    std::fs::metadata(pipelines.join("pipeline-descriptions.bin")).map_or(0, |m| m.len())
}

pub fn run(paths: Vec<PathBuf>, jobs: Option<usize>, force: bool) -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let shaders = cfg.cache_subdir("shaders")?;
    let pipelines = shaders.with_file_name("pipelines");
    let stamp = cfg.cache_dir.join(STAMP);
    victory_hle::modules::d3d12x::use_shader_cache(shaders);
    // Makes the host device first, so the warm-up translates for its facts as a run does.
    let key = victory_hle::modules::d3d12x::precompile_key().map_err(anyhow::Error::msg)?;
    let paths = crate::default_paths(paths)?;
    // shortcut: the paths' names only, a game update in place is not seen; its new
    // shaders translate in the run and get recorded. Add file times if that matters.
    let joined = paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(";");
    let now = Stamp { key, paths: joined, recorded: recorded(&pipelines) };
    let old = std::fs::read_to_string(&stamp).ok().and_then(|t| Stamp::parse(&t));
    let (warm, prebuild) = if force { (true, now.recorded > 0) } else { steps(old.as_ref(), &now) };
    if !warm && !prebuild {
        println!("precompile   up to date");
        return Ok(());
    }
    let threads = crate::default_threads(jobs);
    let complete = if warm {
        crate::translate::warm(paths, Path::new("all"), Some(threads))?
    } else {
        true
    };
    if prebuild {
        let summary = victory_hle::modules::d3d12x::prebuild_pipelines(threads).map_err(anyhow::Error::msg)?;
        println!("{summary}");
    }
    // A warm-up stopped at its time limit goes on at the next run.
    if complete {
        std::fs::write(&stamp, now.to_text())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(key: &str, paths: &str, recorded: u64) -> Stamp {
        Stamp { key: key.into(), paths: paths.into(), recorded }
    }

    #[test]
    fn stamp_round_trips_and_garbage_is_none() {
        let a = s("ab12", "D:\\g;E:\\t", 4096);
        assert_eq!(Stamp::parse(&a.to_text()), Some(a));
        assert_eq!(Stamp::parse("garbage"), None);
        assert_eq!(Stamp::parse("key=a\npaths=b\nrecorded=x\n"), None);
    }

    #[test]
    fn steps_follow_what_changed() {
        let a = s("k", "p", 100);
        assert_eq!(steps(None, &a), (true, true), "no stamp: both");
        assert_eq!(steps(None, &s("k", "p", 0)), (true, false), "nothing recorded");
        assert_eq!(steps(Some(&a), &a), (false, false), "up to date");
        assert_eq!(steps(Some(&a), &s("k2", "p", 100)), (true, true), "translator, settings or driver");
        assert_eq!(steps(Some(&a), &s("k", "q", 100)), (true, false), "other folders");
        assert_eq!(steps(Some(&a), &s("k", "p", 200)), (false, true), "new pipelines recorded");
    }
}
