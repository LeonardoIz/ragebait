//! The pipeline pre-build (ported from upstream VIctory `9c58a1c`): title
//! pipelines made into the host pipeline library ahead of a run, so that
//! the run loads them from the library instead of compiling them while its
//! frames wait (`make`, the half-second budget).
//!
//! Upstream takes the descriptions from the title's shader pack; here a
//! run records them as the title creates them, which needs no title
//! format. With `VICTORY_PSO_PREBUILD=1`, each title pipeline a draw or
//! dispatch first uses is appended to `pipeline-descriptions.bin` in the
//! pipeline cache folder (its shaders by hash, and its creation stream
//! with the pointers taken out), its shader containers to `containers/`
//! beside it. `victory prebuild` reads them back, rebuilds each creation
//! stream and passes it through the run's own code (`read_stream`,
//! `HostPipeline::new`), so a pipeline gets the library name the run will
//! look for; a description that no longer gives its key is not made.
//!
//! The library is where the made pipelines go and what a run reads, as
//! before; a run started after a translator or driver change finds them
//! there instead of making them. The early list (`library`) still starts
//! them at the title's creation, as before.
//!
//! Left out from upstream: the shader pack and the title formats it needs,
//! the list of what a pre-build already put in the library (a second
//! pre-build loads each from the library again, which is quick), and a
//! pre-build inside a run.

use std::collections::{BTreeMap, HashSet};
use std::ffi::{CStr, CString};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::host::{GraphicsState, HostPipeline};
use super::{Bytecode, Shader};

const FILE: &str = "pipeline-descriptions.bin";
const CONTAINERS: &str = "containers";
/// The file starts with this and the translation settings of the latest
/// run that recorded into it (`settings`), then holds the descriptions.
const MAGIC: &[u8; 8] = b"VPDESC02";
const HEADER: usize = 16;
/// Descriptions kept, at most (as many as the early list).
const MAX: usize = 8192;
/// The input layout subobject.
const INPUT_LAYOUT: u32 = 12;
/// Subobjects whose title pointers are zeroed with the rest of their body:
/// the root signature (0), stream output (7), cached pipeline (19) and view
/// instancing (22). `GraphicsState::read` and the key never read them.
const ZEROED: [u32; 4] = [0, 7, 19, 22];
/// Every stage name a pipeline's shaders carry.
const STAGES: [&str; 8] = [
    "vertex",
    "pixel",
    "domain",
    "hull",
    "geometry",
    "compute",
    "amplification",
    "mesh",
];

/// Whether title pipelines are recorded for `victory prebuild`. Off by
/// default (it writes the title's shader containers into the cache
/// folder); `VICTORY_PSO_PREBUILD=1` records.
pub(crate) fn recording() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PSO_PREBUILD").is_some_and(|v| v == "1"))
}

/// An input element: its semantic name and the 24 bytes after the name.
type Element = (Vec<u8>, [u8; 24]);

/// A creation stream without the title's pointers: shader code (pointer
/// zeroed, size kept) and input elements (pointer zeroed, the elements
/// kept beside it). Empty for a plain graphics or compute description.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Portable {
    stream: Vec<u8>,
    elements: Vec<Element>,
}

/// One recorded title pipeline: its key, its shaders (stage, hash) in the
/// order the stream names them, and its stream.
#[derive(Debug, PartialEq)]
struct Description {
    key: String,
    shaders: Vec<(String, String)>,
    portable: Portable,
}

fn word(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

fn put(bytes: &mut [u8], at: usize, v: u64) {
    bytes[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// The whole subobjects of `stream`: where each lies and its type.
fn pointers(stream: &[u8]) -> (Vec<(usize, u32)>, usize) {
    let (mut found, mut at) = (Vec::new(), 0);
    while at + 8 <= stream.len() {
        let kind = u32::from_le_bytes(stream[at..at + 4].try_into().expect("4 bytes"));
        let Some((size, _)) = super::subobject(kind) else {
            break;
        };
        if size > stream.len() - at {
            break;
        }
        found.push((at, kind));
        at += size;
    }
    // `read_stream` stops where this does: what follows is never read.
    (found, at)
}

fn is_shader(kind: u32) -> bool {
    super::subobject(kind).is_some_and(|(_, stage)| stage.is_some())
}

/// The title's creation stream without its pointers.
///
/// # Safety
/// The stream's input layout points at live elements and names.
pub(crate) unsafe fn portable(stream: &[u8]) -> Portable {
    let (found, end) = pointers(stream);
    let mut stream = stream[..end].to_vec();
    let mut elements = Vec::new();
    for (at, kind) in found {
        let size = super::subobject(kind).expect("a known size").0;
        if ZEROED.contains(&kind) {
            stream[at + 8..at + size].fill(0);
            continue;
        }
        let shader = is_shader(kind);
        if !shader && kind != INPUT_LAYOUT {
            continue;
        }
        let ptr = word(&stream, at + 8);
        if shader {
            if ptr == 0 {
                put(&mut stream, at + 16, 0);
            }
        } else if ptr == 0 {
            stream[at + 16..at + 20].fill(0);
        } else {
            let n =
                u32::from_le_bytes(stream[at + 16..at + 20].try_into().expect("4 bytes")) as usize;
            // SAFETY: the caller's elements, 32 bytes each, name first.
            let raw = unsafe { std::slice::from_raw_parts(ptr as *const u8, n * 32) };
            elements = raw
                .chunks_exact(32)
                .map(|e| {
                    let name = word(e, 0) as *const std::ffi::c_char;
                    // SAFETY: a live name (`GraphicsState::read` reads it too).
                    let name = if name.is_null() {
                        Vec::new()
                    } else {
                        unsafe { CStr::from_ptr(name) }.to_bytes().to_vec()
                    };
                    (name, e[8..32].try_into().expect("24 bytes"))
                })
                .collect();
        }
        put(&mut stream, at + 8, 0);
    }
    Portable { stream, elements }
}

/// A portable stream with pointers again: at `containers` (in stream
/// order) and at its own elements, which live as long as it does.
struct Rebuilt {
    stream: Vec<u8>,
    _names: Vec<CString>,
    _elements: Vec<[u64; 4]>,
}

fn rebuild(p: &Portable, containers: &[Vec<u8>]) -> Option<Rebuilt> {
    let names: Vec<CString> = p
        .elements
        .iter()
        .map(|(n, _)| CString::new(n.clone()).ok())
        .collect::<Option<_>>()?;
    let elements: Vec<[u64; 4]> = (p.elements.iter().zip(&names))
        .map(|((_, rest), name)| {
            let mut e = [name.as_ptr() as u64, 0, 0, 0];
            for (i, w) in rest.chunks_exact(8).enumerate() {
                e[i + 1] = u64::from_le_bytes(w.try_into().expect("8 bytes"));
            }
            e
        })
        .collect();
    let mut stream = p.stream.clone();
    let mut next = containers.iter();
    for (at, kind) in pointers(&p.stream).0 {
        if is_shader(kind) {
            if word(&stream, at + 16) != 0 {
                let c = next.next()?;
                put(&mut stream, at + 8, c.as_ptr() as u64);
                put(&mut stream, at + 16, c.len() as u64);
            }
        } else if kind == INPUT_LAYOUT {
            let n =
                u32::from_le_bytes(stream[at + 16..at + 20].try_into().expect("4 bytes")) as usize;
            if n != elements.len() {
                return None;
            }
            if n != 0 {
                put(&mut stream, at + 8, elements.as_ptr() as u64);
            }
        }
    }
    next.next().is_none().then_some(Rebuilt {
        stream,
        _names: names,
        _elements: elements,
    })
}

fn encode(d: &Description) -> Vec<u8> {
    let mut out = Vec::new();
    let mut bytes = |b: &[u8]| {
        out.extend_from_slice(&(b.len() as u32).to_le_bytes());
        out.extend_from_slice(b);
    };
    bytes(d.key.as_bytes());
    bytes(&(d.shaders.len() as u32).to_le_bytes());
    for (stage, hash) in &d.shaders {
        bytes(stage.as_bytes());
        bytes(hash.as_bytes());
    }
    bytes(&d.portable.stream);
    bytes(&(d.portable.elements.len() as u32).to_le_bytes());
    for (name, rest) in &d.portable.elements {
        bytes(name);
        bytes(rest);
    }
    out
}

fn decode(mut b: &[u8]) -> Option<Description> {
    let mut take = || -> Option<&[u8]> {
        let n = u32::from_le_bytes(b.get(..4)?.try_into().ok()?) as usize;
        let part = b.get(4..4 + n)?;
        b = &b[4 + n..];
        Some(part)
    };
    let text = |p: &[u8]| String::from_utf8(p.to_vec()).ok();
    let count = |p: &[u8]| Some(u32::from_le_bytes(p.try_into().ok()?) as usize);
    let key = text(take()?)?;
    let n = count(take()?)?;
    let shaders = (0..n)
        .map(|_| Some((text(take()?)?, text(take()?)?)))
        .collect::<Option<_>>()?;
    let stream = take()?.to_vec();
    let n = count(take()?)?;
    let elements = (0..n)
        .map(|_| Some((take()?.to_vec(), take()?.try_into().ok()?)))
        .collect::<Option<_>>()?;
    let d = Description {
        key,
        shaders,
        portable: Portable { stream, elements },
    };
    // Nothing left over; hashes name files, so they are hex only.
    (take().is_none()
        && d.shaders
            .iter()
            .all(|(_, h)| h.len() == 16 && h.bytes().all(|c| c.is_ascii_hexdigit())))
    .then_some(d)
}

/// What the translation of the shaders depends on besides the translator
/// itself (the `VICTORY_*` switches set, the host's wave size and view
/// heap, and the Wave64 lists), hashed: the library names a pre-build
/// gives are the run's only when this is the run's.
fn settings() -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    h.update(&victory_shader::emit::translation_settings());
    for name in ["VICTORY_WAVE_WIDE_SEEN", "VICTORY_WAVE_WIDE_LIST"] {
        h.update(name.as_bytes());
        h.update(std::env::var_os(name).unwrap_or_default().as_encoded_bytes());
        h.update(&[0]);
    }
    h.finalize().as_bytes()[..8].try_into().expect("8 bytes")
}

/// The translation settings in the header of `dir`'s file, if it has one.
fn recorded_settings(dir: &Path) -> Option<[u8; 8]> {
    let mut header = [0u8; HEADER];
    std::fs::File::open(dir.join(FILE))
        .and_then(|mut f| f.read_exact(&mut header))
        .ok()?;
    header
        .starts_with(MAGIC)
        .then(|| header[8..].try_into().expect("8 bytes"))
}

/// The descriptions in `dir`, each key once, and where the last whole
/// record ends (0: no file, or one of another format). A record cut short
/// (a run that ended during a write) ends the list.
fn read_all(dir: &Path) -> (Vec<Description>, usize) {
    let Ok(bytes) = std::fs::read(dir.join(FILE)) else {
        return (Vec::new(), 0);
    };
    if bytes.len() < HEADER || !bytes.starts_with(MAGIC) {
        return (Vec::new(), 0);
    }
    let mut rest = &bytes[HEADER..];
    let (mut seen, mut all) = (HashSet::new(), Vec::new());
    while let Some(n) = rest
        .get(..4)
        .map(|n| u32::from_le_bytes(n.try_into().expect("4 bytes")) as usize)
    {
        let Some(d) = rest.get(4..4 + n).and_then(decode) else {
            break;
        };
        rest = &rest[4 + n..];
        if seen.insert(d.key.clone()) {
            all.push(d);
        }
    }
    (all, bytes.len() - rest.len())
}

/// The keys `dir` holds. A record cut short is cut off the file, or every
/// record appended after it would be out of reach for good (and appended
/// again by every run).
fn load(dir: &Path) -> HashSet<String> {
    let (all, end) = read_all(dir);
    if end > 0 {
        let cut = std::fs::OpenOptions::new()
            .write(true)
            .open(dir.join(FILE))
            .and_then(|f| {
                if f.metadata()?.len() > end as u64 {
                    tracing::warn!(end, "pipeline descriptions: a record cut short is cut off");
                    f.set_len(end as u64)?;
                }
                Ok(())
            });
        if let Err(e) = cut {
            tracing::warn!(error = %e, "pipeline descriptions: a record cut short stays");
        }
    }
    all.into_iter().map(|d| d.key).collect()
}

/// Descriptions of pipelines first used since the last save, with their
/// shaders (whose containers are written beside them).
type Queued = (Description, Vec<Arc<Shader>>);
static QUEUE: Mutex<Vec<Queued>> = Mutex::new(Vec::new());

/// A draw or dispatch first used the title pipeline `key`: its description
/// goes out with the next save (`library::save`).
pub(crate) fn note(key: &str, shaders: &[Arc<Shader>], record: Option<&Portable>) {
    let Some(portable) = record else { return };
    let d = Description {
        key: key.to_owned(),
        shaders: shaders
            .iter()
            .map(|s| (s.stage.to_owned(), s.hash.clone()))
            .collect(),
        portable: portable.clone(),
    };
    QUEUE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push((d, shaders.to_vec()));
}

/// Appends the queued descriptions that `dir` does not hold yet; on a
/// worker, with the library's save.
pub(crate) fn flush(dir: &Path) {
    let queued = std::mem::take(&mut *QUEUE.lock().unwrap_or_else(|e| e.into_inner()));
    if !queued.is_empty() {
        append(dir, queued, settings());
    }
}

/// Appends those of `queued` that `dir` does not hold yet, the header
/// saying the translation settings `now`. A key counts as held once it is
/// written.
fn append(dir: &Path, queued: Vec<Queued>, now: [u8; 8]) {
    static KNOWN: Mutex<Option<(PathBuf, HashSet<String>)>> = Mutex::new(None);
    static FULL: std::sync::Once = std::sync::Once::new();
    let mut known = KNOWN.lock().unwrap_or_else(|e| e.into_inner());
    if known.as_ref().is_none_or(|(d, _)| d != dir) {
        *known = Some((dir.to_owned(), load(dir)));
    }
    let known = &mut known.as_mut().expect("just set").1;
    let (mut out, mut keys) = (Vec::new(), HashSet::new());
    for (d, shaders) in queued {
        if known.len() + keys.len() >= MAX {
            FULL.call_once(|| {
                tracing::warn!(MAX, "pipeline descriptions: the file is full, no more are recorded (said once)");
            });
            break;
        }
        if known.contains(&d.key) || keys.contains(&d.key) {
            continue;
        }
        if let Err(e) = shaders.iter().try_for_each(|s| keep_container(dir, s)) {
            tracing::warn!(error = %e, "pipeline description not recorded");
            continue;
        }
        let body = encode(&d);
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(&body);
        keys.insert(d.key);
    }
    if out.is_empty() {
        return;
    }
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(dir.join(FILE))?;
        let mut header = [0u8; HEADER];
        let whole = file.read_exact(&mut header).is_ok() && header.starts_with(MAGIC);
        if !whole {
            // A file of another format is started again.
            file.set_len(0)?;
        }
        if !whole || header[8..] != now {
            // The latest run's settings: what a pre-build is for.
            file.seek(SeekFrom::Start(0))?;
            file.write_all(MAGIC)?;
            file.write_all(&now)?;
        }
        file.seek(SeekFrom::End(0))?;
        file.write_all(&out)
    })();
    match written {
        Ok(()) => known.extend(keys),
        Err(e) => tracing::warn!(error = %e, "pipeline descriptions not recorded"),
    }
}

/// Writes a shader's container under its hash, once.
fn keep_container(dir: &Path, s: &Shader) -> std::io::Result<()> {
    let path = dir.join(CONTAINERS).join(&s.hash);
    if path.exists() {
        return Ok(());
    }
    std::fs::create_dir_all(path.parent().expect("a folder"))?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, &s.container)?;
    std::fs::rename(&tmp, &path)
}

/// A container of its own (the pre-build keeps none in the shared table).
fn own(stage: &'static str, bytes: &[u8]) -> Arc<Shader> {
    Arc::new(Shader::new(stage, bytes, &blake3::hash(bytes)))
}

/// A description's shaders (of their own, so the pre-build keeps no
/// container) and graphics state, as the title's creation gives them.
fn replay(
    dir: &Path,
    d: &Description,
) -> Result<(Vec<Arc<Shader>>, Option<Box<GraphicsState>>), &'static str> {
    let containers: Vec<Vec<u8>> = (d.shaders.iter())
        .map(|(_, hash)| {
            let bytes = std::fs::read(dir.join(CONTAINERS).join(hash)).ok()?;
            (blake3::hash(&bytes).to_hex()[..16] == **hash).then_some(bytes)
        })
        .collect::<Option<_>>()
        .ok_or("a shader container is missing or damaged")?;
    let (shaders, graphics) = if d.portable.stream.is_empty() {
        let shaders = (d.shaders.iter().zip(&containers))
            .map(|((stage, _), c)| {
                Some(own(STAGES.iter().find(|s| *s == stage)?, c))
            })
            .collect::<Option<Vec<_>>>()
            .ok_or("unknown stage")?;
        (shaders, None)
    } else {
        let rebuilt =
            rebuild(&d.portable, &containers).ok_or("the stream does not fit its shaders")?;
        let mut g = GraphicsState::default();
        let own = |stage, code: &Bytecode| {
            // SAFETY: the code points into `containers`.
            (!code.code.is_null() && code.size != 0)
                .then(|| own(stage, unsafe { std::slice::from_raw_parts(code.code, code.size) }))
        };
        // SAFETY: a whole subobject lies at `at`, its pointers at `rebuilt`'s own data.
        let shaders =
            super::read_stream(&rebuilt.stream, own, |kind, at| unsafe { g.read(kind, at) });
        (shaders, Some(Box::new(g)))
    };
    if super::make::key(&shaders, graphics.as_deref()) != d.key {
        return Err("the description no longer gives its key");
    }
    Ok((shaders, graphics))
}

/// `victory prebuild`: every recorded title pipeline made into the
/// library (or found there) on `threads` threads, the library saved; the
/// summary line. A pipeline that cannot be made is counted, never fatal.
pub(crate) fn run(threads: usize) -> Result<String, String> {
    let started = std::time::Instant::now();
    if super::library::BUSY.load(Ordering::Relaxed) {
        return Err("the pipeline cache folder is in use by a running game (or another prebuild): close it first".into());
    }
    let dir =
        super::library::dir().ok_or("no pipeline cache folder (cache_dir in victory.toml)")?;
    crate::host::host().ok_or("no host D3D12 device")?;
    // The host facts the translation reads, as a run's first shader has them.
    crate::descriptor::host::probe_view_limit();
    let mismatch = recorded_settings(dir).is_some_and(|r| r != settings());
    if mismatch {
        tracing::warn!(
            "pipeline prebuild: the translation settings (VICTORY_* shader switches, host wave size, view heap) differ from the recording run's; the run finds these pipelines only if started with this environment"
        );
    }
    // Read now, so the library's save keeps the early list as it is.
    super::library::wanted_early("");
    let all = read_all(dir).0;
    // A save writes the whole library (up to 1.35 GB): every few minutes
    // and at the end, not every 20 s.
    const SAVE_EVERY: std::time::Duration = std::time::Duration::from_secs(300);
    let last_save = Mutex::new(std::time::Instant::now());
    let (next, made, loaded) = (AtomicUsize::new(0), AtomicU64::new(0), AtomicU64::new(0));
    let failed: Mutex<BTreeMap<&str, u64>> = Mutex::new(BTreeMap::new());
    // Without a run's log lines per shader and pipeline: thousands of them.
    let quiet = tracing::Dispatch::none();
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, all.len().max(1)) {
            s.spawn(|| {
                while let Some(d) = all.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let outcome = replay(dir, d).and_then(|(shaders, graphics)| {
                        let pipeline = tracing::dispatcher::with_default(&quiet, || {
                            HostPipeline::new(&shaders, graphics.as_deref())
                        });
                        match pipeline {
                            Some(_) if super::library::LOADED_HERE.get() => Ok(&loaded),
                            Some(_) => Ok(&made),
                            None if shaders.iter().any(|s| s.translated().is_none()) => {
                                Err("a shader does not translate")
                            }
                            None => Err("the host refuses it"),
                        }
                    });
                    match outcome {
                        Ok(count) => drop(count.fetch_add(1, Ordering::Relaxed)),
                        Err(why) => {
                            *failed
                                .lock()
                                .unwrap_or_else(|e| e.into_inner())
                                .entry(why)
                                .or_default() += 1
                        }
                    }
                    let due = {
                        let mut t = last_save.lock().unwrap_or_else(|e| e.into_inner());
                        let due = t.elapsed() >= SAVE_EVERY;
                        if due {
                            *t = std::time::Instant::now();
                        }
                        due
                    };
                    if due {
                        super::library::save(true);
                    }
                }
            });
        }
    });
    super::library::save(true);
    let failed = failed.into_inner().unwrap_or_else(|e| e.into_inner());
    let summary = format!(
        "pipeline prebuild: {} recorded, prebuilt {}, loaded from cache {}, failed {}, {:.1} s{}",
        all.len(),
        made.into_inner(),
        loaded.into_inner(),
        failed.values().sum::<u64>(),
        started.elapsed().as_secs_f64(),
        if mismatch {
            "; WARNING: translation settings differ from the recording run's"
        } else {
            ""
        }
    );
    tracing::info!(?failed, "{summary}");
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A title's stream: vertex and pixel shaders, an input layout, a
    /// topology and a rasterizer (cull mode `cull`).
    fn stream(vs: &[u8], ps: &[u8], elements: &[[u64; 4]], cull: u32) -> Vec<u8> {
        let mut s = vec![0u8; 128];
        put(&mut s, 0, 1);
        put(&mut s, 8, vs.as_ptr() as u64);
        put(&mut s, 16, vs.len() as u64);
        put(&mut s, 24, 2);
        put(&mut s, 32, ps.as_ptr() as u64);
        put(&mut s, 40, ps.len() as u64);
        put(&mut s, 48, u64::from(INPUT_LAYOUT));
        put(&mut s, 56, elements.as_ptr() as u64);
        put(&mut s, 64, elements.len() as u64);
        put(&mut s, 72, 14 | 3 << 32);
        put(&mut s, 80, 10 | 3 << 32); // solid fill
        s[88..92].copy_from_slice(&cull.to_le_bytes());
        s
    }

    fn title_key(stream: &[u8]) -> String {
        let mut g = GraphicsState::default();
        let own = |stage, code: &Bytecode| {
            // SAFETY: the test's live code.
            Some(own(stage, unsafe {
                std::slice::from_raw_parts(code.code, code.size)
            }))
        };
        // SAFETY: whole subobjects with live pointers.
        let shaders =
            super::super::read_stream(stream, own, |kind, at| unsafe { g.read(kind, at) });
        super::super::make::key(&shaders, Some(&g))
    }

    fn description(stream: &[u8], vs: &[u8], ps: &[u8]) -> Description {
        let hash = |b: &[u8]| blake3::hash(b).to_hex()[..16].to_owned();
        Description {
            key: title_key(stream),
            shaders: vec![("vertex".into(), hash(vs)), ("pixel".into(), hash(ps))],
            // SAFETY: the test's live elements.
            portable: unsafe { portable(stream) },
        }
    }

    #[test]
    fn a_description_round_trips_and_gives_the_title_key() {
        let (vs, ps) = (vec![7u8; 9], vec![5u8; 6]);
        let name = c"TEXCOORD";
        let elements = [[name.as_ptr() as u64, 2 | 16 << 32, 1 | 8 << 32, 0]];
        let s = stream(&vs, &ps, &elements, 3);
        let d = description(&s, &vs, &ps);
        assert_eq!(
            d.portable.elements,
            vec![(
                b"TEXCOORD".to_vec(),
                elements[0][1..]
                    .iter()
                    .flat_map(|w| w.to_le_bytes())
                    .collect::<Vec<_>>()
                    .try_into()
                    .unwrap()
            )]
        );
        assert_eq!(decode(&encode(&d)), Some(description(&s, &vs, &ps)));
        // Damage or a short record is refused, not a panic.
        let body = encode(&d);
        assert!(decode(&body[..body.len() - 1]).is_none());

        // Written and read back with the containers: the run's key again.
        let dir = tempfile::tempdir().unwrap();
        let shaders = vec![own("vertex", &vs), own("pixel", &ps)];
        append(dir.path(), vec![(d, shaders)], [1; 8]);
        let (all, _) = read_all(dir.path());
        assert_eq!(all, vec![description(&s, &vs, &ps)]);
        let (again, graphics) = replay(dir.path(), &all[0]).unwrap();
        assert_eq!(
            again.iter().map(|s| s.stage).collect::<Vec<_>>(),
            ["vertex", "pixel"]
        );
        assert!(graphics.unwrap().describe().contains("TEXCOORD"));
        // A record cut short ends the list without losing the ones before.
        let path = dir.path().join(FILE);
        let mut bytes = std::fs::read(&path).unwrap();
        bytes.extend_from_slice(&100u32.to_le_bytes());
        std::fs::write(&path, &bytes).unwrap();
        assert_eq!(read_all(dir.path()).0.len(), 1);
    }

    /// A record cut short is cut off when the keys are first read, so the
    /// records appended later are read back.
    #[test]
    fn a_record_cut_short_is_cut_off() {
        let (vs, ps) = (vec![3u8; 8], vec![4u8; 8]);
        let shaders = || vec![own("vertex", &vs), own("pixel", &ps)];
        let (a, b) = (stream(&vs, &ps, &[], 3), stream(&vs, &ps, &[], 2));
        let dir = tempfile::tempdir().unwrap();
        append(dir.path(), vec![(description(&a, &vs, &ps), shaders())], [1; 8]);
        let path = dir.path().join(FILE);
        let whole = std::fs::read(&path).unwrap();
        let mut torn = whole.clone();
        torn.extend_from_slice(&500u32.to_le_bytes());
        torn.extend_from_slice(&[9; 20]);
        std::fs::write(&path, &torn).unwrap();
        assert_eq!(load(dir.path()).len(), 1);
        assert_eq!(std::fs::read(&path).unwrap(), whole);
        // Torn again, and another folder in between, so that `append`
        // reads this one's keys afresh as a new run would.
        std::fs::write(&path, &torn).unwrap();
        let other = tempfile::tempdir().unwrap();
        append(other.path(), vec![(description(&a, &vs, &ps), shaders())], [1; 8]);
        append(dir.path(), vec![(description(&b, &vs, &ps), shaders())], [1; 8]);
        let keys: Vec<String> = read_all(dir.path()).0.into_iter().map(|d| d.key).collect();
        assert_eq!(keys, [title_key(&a), title_key(&b)]);
    }

    /// The header keeps the latest recording run's translation settings,
    /// which a pre-build compares with its own; the records stay.
    #[test]
    fn the_header_tells_other_translation_settings() {
        let (vs, ps) = (vec![6u8; 8], vec![8u8; 8]);
        let shaders = || vec![own("vertex", &vs), own("pixel", &ps)];
        let (a, b) = (stream(&vs, &ps, &[], 3), stream(&vs, &ps, &[], 1));
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(recorded_settings(dir.path()), None);
        append(dir.path(), vec![(description(&a, &vs, &ps), shaders())], [1; 8]);
        assert_eq!(recorded_settings(dir.path()), Some([1; 8]));
        append(dir.path(), vec![(description(&b, &vs, &ps), shaders())], [2; 8]);
        assert_eq!(recorded_settings(dir.path()), Some([2; 8]));
        assert_eq!(read_all(dir.path()).0.len(), 2);
        assert_eq!(settings(), settings());
    }

    /// The key depends on what the pipeline is, not where the title kept
    /// it: copies at other addresses give the same key; another state
    /// another one.
    #[test]
    fn keys_do_not_depend_on_addresses() {
        let name = c"POSITION";
        let (vs, ps) = (vec![1u8; 12], vec![2u8; 4]);
        let (vs2, ps2, name2) = (vs.clone(), ps.clone(), CString::from(name));
        let a = stream(&vs, &ps, &[[name.as_ptr() as u64, 0, 0, 0]], 3);
        let b = stream(&vs2, &ps2, &[[name2.as_ptr() as u64, 0, 0, 0]], 3);
        assert_ne!(a, b);
        assert_eq!(title_key(&a), title_key(&b));
        // SAFETY: live elements.
        assert_eq!(unsafe { portable(&a) }, unsafe { portable(&b) });
        assert_ne!(
            title_key(&a),
            title_key(&stream(&vs, &ps, &[[name.as_ptr() as u64, 0, 0, 0]], 2))
        );
    }
}
