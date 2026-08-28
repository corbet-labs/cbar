// icons.rs — decode each icon once in the life of the machine, not once per launch.
//
// WHY THIS EXISTS. Measured headlessly on a 191-application inventory, with the inventory served
// from a file so no SSH is in the path:
//
//   191 apps, icons blanked    0.095s   <- exec, dynamic link, GTK init, window, grid
//   191 apps, with icons       0.170s   <- the same, plus icons
//
// Icons are three quarters of everything above the floor, and the grid itself is about ten
// milliseconds of it. That cost is paid on EVERY launch to produce a result that is identical
// every time: the same 95 distinct names resolve to the same files and rasterise to the same
// twenty-pixel images until somebody installs software.
//
// So it is paid once and written down. What lands here is the finished pixels -- not the PNG, not
// the SVG, not a path to either -- so a warm launch does no theme lookup, opens no image file,
// runs no rasteriser, and never loads librsvg at all. 95 icons at 20px is 152kB on disk.
//
// ── WHY A WHOLE-CACHE STAMP RATHER THAN PER-ENTRY VALIDATION ────────────────────────────────
//
// Checking each icon against its source file would mean asking the icon theme where every icon
// lives, and that lookup is a large part of the cost this exists to avoid -- a cache that has to
// do the expensive thing to find out whether it is valid saves nothing. Instead the whole file
// carries one stamp taken from the icon THEME's own index files. Installing or removing software
// rewrites those indexes, so the stamp moves and the cache is discarded wholesale. Between
// installs it is trusted completely.
//
// The failure mode this chooses is the right way round: a stale cache shows a stale icon until the
// next package operation, while an over-eager check costs latency on every launch forever.
//
// ONE CLASS OF ENTRY IS EXEMPT, and stating it is the point of this paragraph. An `Icon=` may name
// an absolute FILE rather than a theme icon, and such a file is reachable by no icon theme index --
// so the theme stamp cannot see it change, and the argument above simply does not hold for it.
// Those entries carry the file's own modification time in their key instead, which costs one stat
// on a path we were about to open anyway and makes them self-validating rather than trusted.
use gtk4::prelude::*;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// Bumped whenever the on-disk layout changes, so an old file is discarded rather than
/// misread. A cache that cannot be parsed is simply absent -- never a reason to fail to start.
const MAGIC: &[u8; 6] = b"CBLI04";
const MAX_ICON_CACHE_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESIDENT_ICON_BYTES: usize = 128 * 1024 * 1024;
const MAX_ICON_CACHE_ENTRIES: usize = 32_768;
const MAX_ICON_SOURCE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_ICON_THEME_ROOTS: usize = 256;
const MAX_ICON_THEME_DIRECTORIES: usize = 4_096;

/// Separates an absolute icon path from the modification time that validates it. U+0001 because a
/// key is otherwise an icon name or a file path, and neither may contain a control character --
/// so no real name can be mistaken for a stamped one, whatever it is called.
const STAMP_SEPARATOR: char = '\u{1}';

pub struct Icons {
    px: i32,
    stamp: u64,
    /// WHERE this cache lives, carried rather than recomputed. Tests need their own file, and the
    /// obvious way to give them one -- pointing XDG_CACHE_HOME somewhere else -- is a race: tests
    /// share a process, so one setting the variable changes it under every other running thread.
    /// That is the same trap the state writers are guarded against, and writing it here anyway is
    /// how this was found. An injected path cannot race.
    path: Option<PathBuf>,
    /// name -> RGBA pixels at `px` square. The persisted form.
    pixels: HashMap<String, Arc<[u8]>>,
    /// name -> the texture handed to GTK, built on first use and kept for the process's life.
    /// Separate from `pixels` because a texture cannot be written to a file and pixels cannot be
    /// drawn; keeping both costs one copy of 1.6kB per icon and saves rebuilding either.
    textures: HashMap<String, Option<gtk4::gdk::Texture>>,
    /// Set when a name was decoded that the file did not have, i.e. the file is worth rewriting.
    dirty: bool,
}

/// What this icon is filed under, which for an absolute path includes when that file last changed.
///
/// A theme icon is keyed by its name and validated wholesale by the theme stamp. An absolute path
/// cannot be: no theme index mentions it, so nothing about installing or removing software moves
/// the stamp when such a file is replaced -- and a game that updates its artwork in place would
/// otherwise show the old picture until something unrelated invalidated the whole cache. Folding
/// the modification time into the key means a rewritten file simply misses and is decoded again.
///
/// A file that cannot be stated keeps its bare path, which is the conservative answer: it will be
/// looked up and fail, rather than being cached under a key that claims to know something.
fn cache_key(name: &str) -> String {
    let path = std::path::Path::new(name);
    if !path.is_absolute() {
        return name.to_string();
    }
    let Some((_, metadata)) = open_absolute_icon(path) else {
        return name.to_string();
    };
    let Ok(modified) = metadata.modified() else {
        return name.to_string();
    };
    let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH) else {
        return name.to_string();
    };
    format!("{name}{STAMP_SEPARATOR}{}", age.as_nanos())
}

fn open_absolute_icon(path: &std::path::Path) -> Option<(std::fs::File, std::fs::Metadata)> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)
        .ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_file() || metadata.len() > MAX_ICON_SOURCE_BYTES {
        return None;
    }
    Some((file, metadata))
}

fn cache_path(px: i32) -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .filter(|path| !path.is_empty())
                .map(|home| PathBuf::from(home).join(".cache"))
        })?;
    Some(
        base.join("cbar")
            .join("launcher")
            .join(format!("icons-{px}.bin")),
    )
}

/// One number for "has the set of installed icons changed".
///
/// The newest mtime across the icon themes' own index files. Package operations rewrite those --
/// it is what `gtk-update-icon-cache` exists to do -- so this moves exactly when the answer to an
/// icon lookup could have changed, and stays still the rest of the time.
#[derive(Clone, Debug)]
pub(crate) struct ThemeSnapshot {
    name: String,
    roots: Vec<PathBuf>,
}

pub(crate) fn capture_theme(theme: &gtk4::IconTheme) -> Option<ThemeSnapshot> {
    let mut roots = theme.search_path();
    roots.sort();
    (roots.len() <= MAX_ICON_THEME_ROOTS).then(|| ThemeSnapshot {
        name: theme.theme_name().to_string(),
        roots,
    })
}

fn theme_stamp(theme: &ThemeSnapshot) -> Option<u64> {
    let mut stamp = std::collections::hash_map::DefaultHasher::new();
    // Switching the active theme changes every lookup without touching any theme index. The old
    // stamp missed that completely and served the previous theme's finished pixels forever.
    theme.name.hash(&mut stamp);
    let mut visited_directories = 0usize;
    for root in &theme.roots {
        root.hash(&mut stamp);
        if let Ok(modified) = std::fs::metadata(root).and_then(|m| m.modified()) {
            modified.hash(&mut stamp);
        }
        let Ok(themes) = std::fs::read_dir(root) else {
            continue;
        };
        for theme in themes.flatten() {
            if visited_directories >= MAX_ICON_THEME_DIRECTORIES {
                // A partial stamp could trust stale pixels when an unvisited theme changes. A
                // pathological theme tree therefore disables persistence instead of weakening
                // invalidation correctness or monopolising GTK's event thread.
                return None;
            }
            visited_directories += 1;
            for f in ["icon-theme.cache", "index.theme"] {
                if let Ok(md) = std::fs::metadata(theme.path().join(f))
                    && let Ok(t) = md.modified()
                {
                    t.hash(&mut stamp);
                }
            }
        }
    }
    Some(stamp.finish())
}

pub(crate) struct PreparedIcons {
    px: i32,
    stamp: u64,
    path: Option<PathBuf>,
    pixels: HashMap<String, Arc<[u8]>>,
}

impl PreparedIcons {
    pub(crate) fn empty(path: Option<PathBuf>, px: i32, stamp: u64) -> Self {
        Self {
            px,
            stamp,
            path,
            pixels: HashMap::new(),
        }
    }

    pub(crate) fn identity(&self) -> (i32, u64) {
        (self.px, self.stamp)
    }

    pub(crate) fn prepare(px: i32, theme: ThemeSnapshot) -> Self {
        let Some(stamp) = theme_stamp(&theme) else {
            return Self::empty(None, px, 0);
        };
        match cache_path(px) {
            Some(path) => Self::load_from(path, px, stamp),
            None => Self::empty(None, px, stamp),
        }
    }

    fn load_from(path: PathBuf, px: i32, stamp: u64) -> Self {
        let mut me = Self::empty(Some(path.clone()), px, stamp);
        let Some(root) = path.parent() else {
            return me;
        };
        let Ok(root_metadata) = std::fs::symlink_metadata(root) else {
            return me;
        };
        if root_metadata.file_type().is_symlink()
            || !root_metadata.is_dir()
            || root_metadata.uid() != unsafe { libc::geteuid() }
            || root_metadata.mode() & 0o077 != 0
        {
            return me;
        }
        let file = match std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(&path)
        {
            Ok(file) => file,
            Err(_) => return me,
        };
        let Ok(metadata) = file.metadata() else {
            return me;
        };
        if !metadata.is_file()
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o077 != 0
            || metadata.len() > MAX_ICON_CACHE_BYTES as u64
        {
            return me;
        }
        let mut buf = Vec::with_capacity(metadata.len() as usize);
        if file
            .take((MAX_ICON_CACHE_BYTES + 1) as u64)
            .read_to_end(&mut buf)
            .is_err()
            || buf.len() > MAX_ICON_CACHE_BYTES
        {
            return me;
        }
        if buf.len() < MAGIC.len() + 12 || &buf[..MAGIC.len()] != MAGIC {
            return me;
        }
        let mut at = MAGIC.len();
        let take = |b: &[u8], at: &mut usize, n: usize| -> Option<Vec<u8>> {
            if *at + n > b.len() {
                return None;
            }
            let v = b[*at..*at + n].to_vec();
            *at += n;
            Some(v)
        };
        let Some(st) = take(&buf, &mut at, 8) else {
            return me;
        };
        if u64::from_le_bytes(st.try_into().unwrap_or([0; 8])) != stamp {
            return me;
        }
        let Some(cnt) = take(&buf, &mut at, 4) else {
            return me;
        };
        let count = u32::from_le_bytes(cnt.try_into().unwrap_or([0; 4])) as usize;
        let Some(bytes) = usize::try_from(px)
            .ok()
            .and_then(|size| size.checked_mul(size))
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return me;
        };
        let resident_entries = MAX_RESIDENT_ICON_BYTES / bytes.max(256);
        let structurally_possible = buf.len().saturating_sub(at) / bytes.saturating_add(2).max(1);
        if count
            > MAX_ICON_CACHE_ENTRIES
                .min(resident_entries)
                .min(structurally_possible)
        {
            return me;
        }
        let mut parsed = HashMap::with_capacity(count);
        for _ in 0..count {
            let Some(nl) = take(&buf, &mut at, 2) else {
                return me;
            };
            let n = u16::from_le_bytes(nl.try_into().unwrap_or([0; 2])) as usize;
            let Some(name) = take(&buf, &mut at, n) else {
                return me;
            };
            let Some(px_data) = take(&buf, &mut at, bytes) else {
                return me;
            };
            let Ok(name) = String::from_utf8(name) else {
                return me;
            };
            if parsed.insert(name, Arc::from(px_data)).is_some() {
                return me;
            }
        }
        me.pixels = parsed;
        me
    }
}

impl Icons {
    pub(crate) fn from_prepared(prepared: PreparedIcons) -> Self {
        Self {
            px: prepared.px,
            stamp: prepared.stamp,
            path: prepared.path,
            pixels: prepared.pixels,
            textures: HashMap::new(),
            dirty: false,
        }
    }

    #[cfg(test)]
    fn load_from(path: PathBuf, px: i32, stamp: u64) -> Self {
        Self::from_prepared(PreparedIcons::load_from(path, px, stamp))
    }

    /// The texture for `name`, from the cache when possible and from the icon theme when not.
    pub fn texture(&mut self, name: &str, theme: &gtk4::IconTheme) -> Option<gtk4::gdk::Texture> {
        if name.is_empty() {
            return None;
        }
        let key = cache_key(name);
        if let Some(hit) = self.textures.get(&key) {
            return hit.clone();
        }
        let px = self.px;
        let (made, new_pixels) = match self.pixels.get(&key) {
            // THE WARM PATH, and the whole point: no lookup, no file, no rasteriser.
            Some(data) => (
                Some(gtk4::gdk::MemoryTexture::new(
                    px,
                    px,
                    gtk4::gdk::MemoryFormat::R8g8b8a8,
                    &gtk4::glib::Bytes::from(data.as_ref()),
                    (px * 4) as usize,
                ))
                .map(|t| t.upcast::<gtk4::gdk::Texture>()),
                None,
            ),
            None => {
                let decoded = decode(name, px, theme);
                let pixels = decoded.as_ref().and_then(|pb| rgba_square(pb, px));
                (
                    decoded.map(|pb| gtk4::gdk::Texture::for_pixbuf(&pb).upcast()),
                    pixels,
                )
            }
        };
        let add_pixels = usize::from(new_pixels.is_some());
        if self.within_resident_budget(add_pixels, 1) {
            if let Some(raw) = new_pixels {
                self.pixels.insert(key.clone(), raw.into());
                self.dirty = true;
            }
            self.textures.insert(key, made.clone());
        }
        made
    }

    fn within_resident_budget(&self, add_pixels: usize, add_textures: usize) -> bool {
        let bytes = usize::try_from(self.px)
            .ok()
            .and_then(|size| size.checked_mul(size))
            .and_then(|pixels| pixels.checked_mul(4))
            // Even a missing-icon entry owns a key/hash bucket/Option; charge a small floor so a
            // hostile stream of unique missing names cannot bypass the pixel budget.
            .map(|bytes| bytes.max(256));
        bytes
            .and_then(|bytes| {
                self.pixels
                    .len()
                    .checked_add(add_pixels)?
                    .checked_add(self.textures.len())?
                    .checked_add(add_textures)?
                    .checked_mul(bytes)
            })
            .is_some_and(|bytes| bytes <= MAX_RESIDENT_ICON_BYTES)
    }

    /// Write the file if anything new was decoded. Atomic, for the same reason the state files are:
    /// a truncated cache is worse than none, because it parses as "these icons are blank".
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let Some(path) = self.path.as_ref() else {
            return;
        };
        let Some(bytes) = usize::try_from(self.px)
            .ok()
            .and_then(|size| size.checked_mul(size))
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return;
        };
        let usable: Vec<(String, Arc<[u8]>)> = self
            .pixels
            .iter()
            .filter(|(n, d)| d.len() == bytes && n.len() <= u16::MAX as usize)
            .map(|(name, data)| (name.clone(), data.clone()))
            .collect();
        let stamp = self.stamp;
        // Any icons decoded after this snapshot set the bit again, while ordinary redraws remain
        // a no-op. The immutable Arc-backed pixels keep serialization off GTK without copying the
        // full cache on the event thread.
        self.dirty = false;
        let write = IconWrite {
            stamp,
            bytes_per_icon: bytes,
            pixels: usable,
        };
        if cfg!(test) {
            serialize_and_write_cache(path, write);
            return;
        }
        enqueue_cache_write(path.clone(), write);
    }
}

struct IconWrite {
    stamp: u64,
    bytes_per_icon: usize,
    pixels: Vec<(String, Arc<[u8]>)>,
}

#[derive(Default)]
struct PendingIconWrite {
    latest: Option<IconWrite>,
}

/// Serialize writes per cache path and retain only the newest snapshot waiting behind an active
/// write. Provider B can therefore never rename an older snapshot after provider C's newer one;
/// intermediate snapshots are disposable, but write order is monotonic.
fn enqueue_cache_write(path: PathBuf, write: IconWrite) {
    enqueue_cache_write_with(path, write, || {}, serialize_and_write_cache);
}

fn enqueue_cache_write_with<Before, Writer>(
    path: PathBuf,
    write: IconWrite,
    before_first_write: Before,
    writer: Writer,
) where
    Before: FnOnce() + Send + 'static,
    Writer: Fn(&std::path::Path, IconWrite) + Send + 'static,
{
    static WRITES: OnceLock<Mutex<HashMap<PathBuf, PendingIconWrite>>> = OnceLock::new();
    let writes = WRITES.get_or_init(|| Mutex::new(HashMap::new()));
    let Ok(mut pending) = writes.lock() else {
        return;
    };
    if let Some(active) = pending.get_mut(&path) {
        active.latest = Some(write);
        return;
    }
    pending.insert(
        path.clone(),
        PendingIconWrite {
            latest: Some(write),
        },
    );
    drop(pending);

    let worker_path = path.clone();
    if std::thread::Builder::new()
        .name("cbar-launcher-icons".into())
        .spawn(move || {
            let mut before_first_write = Some(before_first_write);
            loop {
                let next = {
                    let Ok(mut pending) = writes.lock() else {
                        return;
                    };
                    let Some(active) = pending.get_mut(&worker_path) else {
                        return;
                    };
                    match active.latest.take() {
                        Some(write) => write,
                        None => {
                            pending.remove(&worker_path);
                            return;
                        }
                    }
                };
                if let Some(before) = before_first_write.take() {
                    before();
                }
                writer(&worker_path, next);
            }
        })
        .is_err()
        && let Ok(mut pending) = writes.lock()
    {
        pending.remove(&path);
    }
}

fn serialize_and_write_cache(path: &std::path::Path, write: IconWrite) {
    let total = write
        .pixels
        .iter()
        .try_fold(MAGIC.len() + 12, |total, (name, _)| {
            total
                .checked_add(2)
                .and_then(|total| total.checked_add(name.len()))
                .and_then(|total| total.checked_add(write.bytes_per_icon))
        });
    let Some(total) = total.filter(|total| *total <= MAX_ICON_CACHE_BYTES) else {
        return;
    };
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&write.stamp.to_le_bytes());
    out.extend_from_slice(&(write.pixels.len() as u32).to_le_bytes());
    for (name, data) in write.pixels {
        out.extend_from_slice(&(name.len() as u16).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(&data);
    }
    write_cache(path, &out);
}

fn write_cache(path: &std::path::Path, out: &[u8]) {
    if let Some(dir) = path.parent() {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let Ok(metadata) = std::fs::symlink_metadata(dir) else {
            return;
        };
        if metadata.file_type().is_symlink()
            || !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
        {
            return;
        }
        if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_err() {
            return;
        }
    }
    static NEXT_TMP: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!(
        "bin.{}.{}.tmp",
        std::process::id(),
        NEXT_TMP.fetch_add(1, Ordering::Relaxed)
    ));
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)
        && file.write_all(out).is_ok()
        && file.sync_all().is_ok()
        && std::fs::rename(&tmp, path).is_ok()
    {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    let _ = std::fs::remove_file(tmp);
}

/// Resolve a name through the icon theme and rasterise it to `px` square.
///
/// This is the slow path, and everything above exists to run it as rarely as possible. Vector and
/// raster take the same route deliberately: `IconPaintable` renders an SVG at the size asked for
/// and a PNG at the file's own size, so going through the pixbuf loader for both is what makes the
/// result uniformly small rather than uniformly whatever upstream shipped.
fn decode(name: &str, px: i32, theme: &gtk4::IconTheme) -> Option<gtk4::gdk_pixbuf::Pixbuf> {
    // Icon= may be either a theme name or an absolute file. Passing a file path to
    // IconTheme::lookup_icon does not load that file; it searches for a theme icon literally
    // carrying all those slash-separated characters and returns the missing-image paintable.
    // Steam and standalone-game entries legitimately use absolute PNG paths, so recognise that
    // half of the desktop-entry contract before asking the theme about ordinary names.
    let candidate = PathBuf::from(name);
    if candidate.is_absolute() {
        // IS IT THERE. `is_absolute` is a purely lexical test, and an entry naming a file that has
        // since been uninstalled would otherwise be handed to the loader as if it existed. Asking
        // the theme about it instead would be worse than useless: no theme contains an icon called
        // `/opt/something/icon.png`, so the lookup can only return the missing-image paintable.
        let (file, metadata) = open_absolute_icon(&candidate)?;
        let mut encoded = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_ICON_SOURCE_BYTES + 1)
            .read_to_end(&mut encoded)
            .ok()?;
        if encoded.len() as u64 > MAX_ICON_SOURCE_BYTES {
            return None;
        }
        let loader = gtk4::gdk_pixbuf::PixbufLoader::new();
        loader.set_size(px, px);
        loader.write(&encoded).ok()?;
        loader.close().ok()?;
        return loader.pixbuf();
    }
    let path = theme
        .lookup_icon(
            name,
            &[],
            px,
            1,
            gtk4::TextDirection::None,
            gtk4::IconLookupFlags::empty(),
        )
        .file()?
        .path()?;
    gtk4::gdk_pixbuf::Pixbuf::from_file_at_size(&path, px, px).ok()
}

/// Pixels as an exactly `px` by `px` RGBA block, which is what the cache format stores and what
/// `MemoryTexture` wants. A loader asked for a size gives back something that FITS it, preserving
/// aspect, so a wide icon comes back short -- centring it here means every entry is one fixed
/// length and the file needs no per-entry geometry.
fn rgba_square(pb: &gtk4::gdk_pixbuf::Pixbuf, px: i32) -> Option<Vec<u8>> {
    let (w, h) = (pb.width(), pb.height());
    if w <= 0 || h <= 0 || w > px || h > px {
        return None;
    }
    let src = pb.read_pixel_bytes();
    let stride = pb.rowstride() as usize;
    let chans = pb.n_channels() as usize;
    if chans != 3 && chans != 4 {
        return None;
    }
    let mut out = vec![0u8; (px * px * 4) as usize];
    let (ox, oy) = (((px - w) / 2) as usize, ((px - h) / 2) as usize);
    for y in 0..h as usize {
        for x in 0..w as usize {
            let s = y * stride + x * chans;
            if s + chans > src.len() {
                return None;
            }
            let d = ((y + oy) * px as usize + (x + ox)) * 4;
            out[d] = src[s];
            out[d + 1] = src[s + 1];
            out[d + 2] = src[s + 2];
            out[d + 3] = if chans == 4 { src[s + 3] } else { 255 };
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "cbar-launcher-icons-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&d).unwrap();
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o700)).unwrap();
        d.join("icons.bin")
    }

    fn blank(path: PathBuf, px: i32, stamp: u64) -> Icons {
        Icons {
            px,
            stamp,
            path: Some(path),
            pixels: HashMap::new(),
            textures: HashMap::new(),
            dirty: true,
        }
    }

    /// A theme icon is filed under its own name, because the theme stamp is what validates it and
    /// a name carries nothing else worth knowing.
    #[test]
    fn a_theme_icon_is_keyed_by_its_name_alone() {
        assert_eq!(cache_key("firefox"), "firefox");
        // Relative paths are not part of the desktop-entry contract and are treated as names, so
        // they must not acquire a stamp either.
        assert_eq!(cache_key("icons/thing.png"), "icons/thing.png");
    }

    /// An absolute path is the one entry the theme stamp cannot see, so it validates itself: the
    /// key moves when the file does, and a rewritten icon is decoded again instead of being served
    /// from the cache until something unrelated invalidates the whole file.
    #[test]
    fn a_rewritten_icon_file_gets_a_new_key() {
        let path = tmp("rewritten").with_file_name("art.png");
        std::fs::write(&path, b"first").unwrap();
        let name = path.to_str().unwrap();
        let first = cache_key(name);
        assert_ne!(first, name, "an absolute path must carry its own stamp");

        // Two writes within one filesystem timestamp tick would be indistinguishable, which is a
        // property of the clock rather than of this code -- so the file is given an explicitly
        // different modification time rather than being raced against it.
        std::fs::write(&path, b"second").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        std::fs::File::open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();

        assert_ne!(
            cache_key(name),
            first,
            "a file rewritten behind an unchanged path must miss the cache"
        );
    }

    /// A path that names nothing is not asked about: the theme cannot hold an icon called
    /// `/opt/thing/icon.png`, so falling through to it could only produce the missing-image glyph.
    #[test]
    fn an_absolute_path_that_is_not_there_keeps_its_bare_name() {
        let missing = "/nonexistent/cbar-launcher-test/icon.png";
        assert_eq!(cache_key(missing), missing);
    }

    /// A file this program did not write, or wrote in an older shape, reads as "no cache" rather
    /// than as garbage pixels.
    #[test]
    fn a_foreign_file_is_ignored() {
        let p = tmp("foreign");
        std::fs::write(&p, b"not a cbar launcher cache at all").unwrap();
        assert!(
            Icons::load_from(p, 20, 1).pixels.is_empty(),
            "garbage must not become icons"
        );
    }

    /// The stamp IS the validity test, so a mismatched one discards everything -- otherwise an
    /// application that was uninstalled keeps its icon forever.
    #[test]
    fn a_stale_stamp_discards_everything() {
        let p = tmp("stale");
        let px = 4;
        let mut w = blank(p.clone(), px, 12345);
        w.pixels
            .insert("thing".into(), vec![7u8; (px * px * 4) as usize].into());
        w.save();

        assert!(
            Icons::load_from(p.clone(), px, 999).pixels.is_empty(),
            "a different stamp reads as empty"
        );
        assert_eq!(
            Icons::load_from(p, px, 12345).pixels.len(),
            1,
            "the matching stamp still reads"
        );
    }

    /// Round trip: what is written comes back byte for byte.
    #[test]
    fn a_written_cache_reads_back_identical() {
        let p = tmp("roundtrip");
        let px = 4;
        let data: Vec<u8> = (0..(px * px * 4)).map(|i| (i % 251) as u8).collect();
        let mut w = blank(p.clone(), px, 77);
        w.pixels.insert("round.trip".into(), data.clone().into());
        w.save();

        assert_eq!(
            Icons::load_from(p, px, 77)
                .pixels
                .get("round.trip")
                .map(AsRef::as_ref),
            Some(data.as_slice())
        );
    }

    /// Nothing new decoded means nothing written -- a warm start must not rewrite the file it just
    /// read, which would be a needless write on the latency path every single launch.
    #[test]
    fn a_clean_cache_is_not_rewritten() {
        let p = tmp("clean");
        let mut w = blank(p.clone(), 4, 5);
        w.pixels.insert("x".into(), vec![0u8; 64].into());
        w.save();
        let before = std::fs::metadata(&p).unwrap().len();
        let mut read_back = Icons::load_from(p.clone(), 4, 5);
        read_back.dirty = false;
        std::fs::write(&p, b"sentinel").unwrap();
        read_back.save();
        assert_eq!(
            std::fs::read(&p).unwrap(),
            b"sentinel",
            "a clean cache wrote nothing"
        );
        assert!(before > 0);
    }

    #[test]
    fn cache_reads_are_bounded_and_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let target = tmp("symlink-read-target");
        std::fs::write(&target, b"not trusted through a symlink").unwrap();
        let link = target.with_file_name("icons-link.bin");
        symlink(&target, &link).unwrap();
        assert!(Icons::load_from(link, 20, 1).pixels.is_empty());

        let huge = tmp("oversized");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_ICON_CACHE_BYTES as u64 + 1)
            .unwrap();
        assert!(Icons::load_from(huge, 20, 1).pixels.is_empty());
    }

    #[test]
    fn cache_load_requires_private_roots_and_files() {
        let path = tmp("unsafe-mode");
        let root = path.parent().unwrap().to_path_buf();
        let original_root_mode = std::fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        let mut cache = blank(path.clone(), 4, 11);
        cache.pixels.insert("x".into(), vec![0u8; 64].into());
        cache.save();

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(Icons::load_from(path.clone(), 4, 11).pixels.is_empty());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(Icons::load_from(path, 4, 11).pixels.is_empty());
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(original_root_mode))
            .unwrap();
    }

    #[test]
    fn cache_entry_count_is_rejected_before_hostile_allocation() {
        let path = tmp("hostile-count");
        let count = MAX_ICON_CACHE_ENTRIES + 1;
        let mut encoded = Vec::with_capacity(MAGIC.len() + 12 + count * 6);
        encoded.extend_from_slice(MAGIC);
        encoded.extend_from_slice(&7u64.to_le_bytes());
        encoded.extend_from_slice(&(count as u32).to_le_bytes());
        // At one pixel an empty key record is two length bytes plus four RGBA bytes. Supplying a
        // structurally plausible body proves rejection is the explicit entry/resident cap, not a
        // coincidentally short file discovered only after allocating the advertised map.
        encoded.resize(encoded.len() + count * 6, 0);
        std::fs::write(&path, encoded).unwrap();
        assert!(Icons::load_from(path, 1, 7).pixels.is_empty());
    }

    #[test]
    fn absolute_icon_sources_are_regular_bounded_and_not_symlinked() {
        use std::os::unix::fs::symlink;

        let regular = tmp("absolute-regular");
        std::fs::write(&regular, b"not an image, but a bounded regular source").unwrap();
        assert!(open_absolute_icon(&regular).is_some());

        let link = tmp("absolute-link");
        symlink(&regular, &link).unwrap();
        assert!(open_absolute_icon(&link).is_none());

        let huge = tmp("absolute-huge");
        std::fs::File::create(&huge)
            .unwrap()
            .set_len(MAX_ICON_SOURCE_BYTES + 1)
            .unwrap();
        assert!(open_absolute_icon(&huge).is_none());
    }

    #[test]
    fn cache_save_refuses_a_symlinked_root() {
        use std::os::unix::fs::symlink;

        let base = tmp("symlink-root").parent().unwrap().join("root");
        let target = base.join("target");
        let link = base.join("cache");
        std::fs::create_dir_all(&target).unwrap();
        symlink(&target, &link).unwrap();
        let mut icons = blank(link.join("icons.bin"), 4, 1);
        icons.pixels.insert("safe".into(), vec![0; 64].into());
        icons.save();
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);

        std::fs::remove_file(link).unwrap();
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn cache_load_refuses_a_symlinked_root() {
        use std::os::unix::fs::symlink;

        let base = tmp("symlink-load-root").parent().unwrap().join("root");
        let target = base.join("target");
        let link = base.join("cache");
        std::fs::create_dir_all(&target).unwrap();
        let target_file = target.join("icons.bin");
        let mut writer = blank(target_file, 4, 5);
        writer
            .pixels
            .insert("thing".into(), vec![7u8; 4 * 4 * 4].into());
        writer.save();
        symlink(&target, &link).unwrap();

        assert!(
            Icons::load_from(link.join("icons.bin"), 4, 5)
                .pixels
                .is_empty()
        );
    }

    #[test]
    fn async_cache_writer_serializes_and_coalesces_to_newest_snapshot() {
        use std::sync::Barrier;

        fn snapshot(stamp: u64) -> IconWrite {
            IconWrite {
                stamp,
                bytes_per_icon: 4,
                pixels: vec![(format!("icon-{stamp}"), vec![stamp as u8; 4].into())],
            }
        }

        let path = tmp("ordered-writer");
        let started = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let seen = Arc::new(Mutex::new(Vec::new()));
        enqueue_cache_write_with(
            path.clone(),
            snapshot(1),
            {
                let started = started.clone();
                let release = release.clone();
                move || {
                    started.wait();
                    release.wait();
                }
            },
            {
                let seen = seen.clone();
                move |_, write| {
                    let stamp = write.stamp;
                    seen.lock().unwrap().push(stamp);
                    if stamp == 3 {
                        done_tx.send(()).unwrap();
                    }
                }
            },
        );
        // Hold snapshot 1 in the single worker while 2 is replaced by 3 in the pending slot.
        started.wait();
        enqueue_cache_write(path.clone(), snapshot(2));
        enqueue_cache_write(path, snapshot(3));
        release.wait();
        done_rx
            .recv_timeout(std::time::Duration::from_secs(2))
            .expect("newest cache snapshot was not written");
        assert_eq!(*seen.lock().unwrap(), [1, 3]);
    }
}
