//! Bounded, cancellable artwork loading, with thumbnail decoding off the UI thread.
use gtk::{gdk, gdk_pixbuf, gio, glib, prelude::*};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    rc::Rc,
    time::Duration,
};
const MAX_BYTES: usize = 2 * 1024 * 1024;
type Pixels = (i32, i32, usize, Vec<u8>);

// GIO may synchronously close an HTTP stream when its last reference drops.
// Keep it alive until pending cancellation settles, then close asynchronously.
struct ArtworkStream(gio::FileInputStream);
impl Drop for ArtworkStream {
    fn drop(&mut self) {
        let stream = self.0.clone();
        glib::MainContext::ref_thread_default().spawn_local(async move {
            while stream.has_pending() {
                glib::timeout_future(Duration::from_millis(10)).await;
            }
            let _ = stream.close_future(glib::Priority::LOW).await;
        });
    }
}

fn decode(bytes: Vec<u8>) -> Result<Pixels, String> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.connect_size_prepared(|loader, width, height| {
        let ratio = (192.0 / width.max(1) as f64)
            .min(192.0 / height.max(1) as f64)
            .min(1.0);
        loader.set_size(
            (width as f64 * ratio).max(1.0) as i32,
            (height as f64 * ratio).max(1.0) as i32,
        );
    });
    loader.write(&bytes).map_err(|e| e.to_string())?;
    loader.close().map_err(|e| e.to_string())?;
    let pixbuf = loader.pixbuf().ok_or("Missing artwork pixels")?;
    let pixbuf = pixbuf
        .add_alpha(false, 0, 0, 0)
        .map_err(|e| e.to_string())?;
    if pixbuf.width() > 192 || pixbuf.height() > 192 {
        return Err("Artwork exceeds thumbnail size".into());
    }
    Ok((
        pixbuf.width(),
        pixbuf.height(),
        pixbuf.rowstride() as usize,
        pixbuf.read_pixel_bytes().to_vec(),
    ))
}
async fn load(url: String) -> Result<Pixels, String> {
    let stream = ArtworkStream(
        gio::File::for_uri(&url)
            .read_future(glib::Priority::LOW)
            .await
            .map_err(|e| e.to_string())?,
    );
    let mut bytes = Vec::new();
    loop {
        let chunk = stream
            .0
            .read_bytes_future(64 * 1024, glib::Priority::LOW)
            .await
            .map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            break;
        }
        if bytes.len() + chunk.len() > MAX_BYTES {
            return Err("Artwork exceeds 2 MiB".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    gio::spawn_blocking(move || decode(bytes))
        .await
        .map_err(|_| "Artwork decoder failed".to_string())?
}
struct Art {
    root: glib::WeakRef<gtk::Box>,
    picture: gtk::Picture,
    proxy: gio::DBusProxy,
    url: RefCell<Option<String>>,
    loaded: RefCell<Option<String>>,
    request: RefCell<Option<gio::Cancellable>>,
    revision: Cell<u64>,
}
impl Drop for Art {
    fn drop(&mut self) {
        if let Some(request) = self.request.get_mut().take() {
            request.cancel();
        }
    }
}
impl Art {
    fn cancel(&self) {
        self.revision.set(self.revision.get().wrapping_add(1));
        if let Some(request) = self.request.borrow_mut().take() {
            request.cancel();
        }
    }
    fn refresh(self: &Rc<Self>) {
        let url = self
            .proxy
            .cached_property("Metadata")
            .and_then(|v| v.get::<BTreeMap<String, glib::Variant>>())
            .and_then(|metadata| metadata.get("mpris:artUrl").and_then(|v| v.get::<String>()))
            .filter(|url| {
                url.len() <= 8192
                    && ["file://", "https://", "http://"]
                        .iter()
                        .any(|scheme| url.starts_with(scheme))
            });
        if *self.url.borrow() != url {
            self.cancel();
            *self.url.borrow_mut() = url;
            self.picture.set_paintable(gdk::Paintable::NONE);
            self.picture.set_visible(false);
            self.loaded.borrow_mut().take();
        }
        self.start();
    }
    fn start(self: &Rc<Self>) {
        if !self.root.upgrade().is_some_and(|root| root.is_mapped())
            || self.request.borrow().is_some()
        {
            return;
        }
        let Some(url) = self.url.borrow().clone() else {
            return;
        };
        if self.loaded.borrow().as_ref() == Some(&url) {
            return;
        }
        let request = gio::Cancellable::new();
        *self.request.borrow_mut() = Some(request.clone());
        let revision = self.revision.get();
        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            let timeout_request = request.clone();
            let timer = glib::timeout_add_local(Duration::from_secs(5), move || {
                timeout_request.cancel();
                glib::ControlFlow::Continue
            });
            let result = gio::CancellableFuture::new(load(url.clone()), request).await;
            timer.remove();
            let Some(state) = weak.upgrade() else { return };
            if state.revision.get() != revision {
                return;
            }
            state.request.borrow_mut().take();
            // Retain failures too: unrelated player signals must not trigger retries.
            *state.loaded.borrow_mut() = Some(url);
            if let Ok(Ok((width, height, stride, pixels))) = result {
                let texture = gdk::MemoryTexture::new(
                    width,
                    height,
                    gdk::MemoryFormat::R8g8b8a8,
                    &glib::Bytes::from_owned(pixels),
                    stride,
                );
                state.picture.set_paintable(Some(&texture));
                state.picture.set_visible(true);
            }
        });
    }
}
pub fn widget(proxy: &gio::DBusProxy) -> gtk::Box {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let picture = gtk::Picture::new();
    picture.set_content_fit(gtk::ContentFit::Contain);
    picture.set_size_request(192, 192);
    picture.set_halign(gtk::Align::Center);
    picture.set_visible(false);
    picture.update_property(&[gtk::accessible::Property::Label("Album artwork")]);
    root.append(&picture);
    let state = Rc::new(Art {
        root: root.downgrade(),
        picture,
        proxy: proxy.clone(),
        url: RefCell::new(None),
        loaded: RefCell::new(None),
        request: RefCell::new(None),
        revision: Cell::new(0),
    });
    let weak = Rc::downgrade(&state);
    proxy.connect_local("g-properties-changed", false, move |_| {
        if let Some(state) = weak.upgrade() {
            state.refresh();
        }
        None
    });
    let weak = Rc::downgrade(&state);
    root.connect_map(move |_| {
        if let Some(state) = weak.upgrade() {
            state.start();
        }
    });
    let weak = Rc::downgrade(&state);
    root.connect_unmap(move |_| {
        if let Some(state) = weak.upgrade() {
            state.cancel();
        }
    });
    state.refresh();
    root.connect_destroy(move |_| {
        let _ = &state;
    });
    root
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires the GIO HTTP backend"]
    fn http_loading_limits_and_cancellation() {
        use std::{
            io::{Read, Write},
            net::TcpListener,
            sync::{
                Arc,
                atomic::{AtomicBool, Ordering},
            },
        };
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = stopped.clone();
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while !stop.load(Ordering::Relaxed) && std::time::Instant::now() < deadline {
                let Ok((mut socket, _)) = listener.accept() else {
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(1)))
                    .unwrap();
                let mut request = [0u8; 4096];
                let count = socket.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..count]);
                let slow = request.contains("/slow");
                let body = if request.contains("/large") {
                    vec![0; MAX_BYTES + 1]
                } else {
                    b"P6\n1 1\n255\n\xc0\x40\x20".to_vec()
                };
                let _ = write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: image/x-portable-pixmap\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                if slow {
                    let _ = started_tx.send(());
                    std::thread::sleep(Duration::from_millis(500));
                }
                let _ = socket.write_all(&body);
            }
        });
        let context = glib::MainContext::new();
        let results = context
            .with_thread_default(|| {
                let image = context.block_on(load(format!("http://{address}/image")));
                let large = context.block_on(load(format!("http://{address}/large")));
                let cancel = gio::Cancellable::new();
                let request = cancel.clone();
                let canceller = std::thread::spawn(move || {
                    let started = started_rx.recv_timeout(Duration::from_secs(2)).is_ok();
                    request.cancel();
                    started
                });
                let cancelled = context.block_on(gio::CancellableFuture::new(
                    load(format!("http://{address}/slow")),
                    cancel,
                ));
                (image, large, cancelled, canceller.join().unwrap())
            })
            .unwrap();
        stopped.store(true, Ordering::Relaxed);
        server.join().unwrap();
        let (image, large, cancelled, started) = results;
        let (width, height, _, pixels) = image.unwrap();
        assert_eq!((width, height), (1, 1));
        assert_eq!(&pixels[..4], &[192, 64, 32, 255]);
        assert_eq!(large.unwrap_err(), "Artwork exceeds 2 MiB");
        assert!(started, "cancel only after the HTTP response has started");
        assert!(
            cancelled.is_err(),
            "cancellation must interrupt the pending body read"
        );
    }

    #[test]
    fn bounded_file_loading_and_thumbnail_decode() {
        let path = std::env::temp_dir().join(format!("wm-art-bounds-{}", std::process::id()));
        let context = glib::MainContext::new();
        context
            .with_thread_default(|| {
                let url = gio::File::for_path(&path).uri().to_string();
                std::fs::write(&path, vec![0; MAX_BYTES + 1]).unwrap();
                assert_eq!(
                    context.block_on(load(url.clone())).unwrap_err(),
                    "Artwork exceeds 2 MiB"
                );
                std::fs::write(&path, b"invalid image").unwrap();
                assert!(context.block_on(load(url.clone())).is_err());
                let mut image = b"P6\n400 200\n255\n".to_vec();
                image.extend([192u8, 64, 32].repeat(400 * 200));
                std::fs::write(&path, image).unwrap();
                let (width, height, stride, pixels) = context.block_on(load(url)).unwrap();
                assert_eq!((width, height), (192, 96));
                assert!(stride >= width as usize * 4);
                assert_eq!(&pixels[..4], &[192, 64, 32, 255]);
                std::fs::remove_file(&path).unwrap();
            })
            .unwrap();
    }
}
