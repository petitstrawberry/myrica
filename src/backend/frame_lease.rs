//! Owner-return lifetime and UI presentation ordering for borrowed GPU frames.
use std::sync::Arc;

/// Dropping the last lease queues a return to the engine owner. It never calls
/// a native WebCore function on the UI or renderer thread.
pub(crate) struct FrameLease {
    pub token: u64,
    release: Arc<dyn Fn(u64) + Send + Sync>,
}
impl FrameLease {
    pub(crate) fn new(token: u64, release: Arc<dyn Fn(u64) + Send + Sync>) -> Arc<Self> {
        Arc::new(Self { token, release })
    }
}
impl Drop for FrameLease {
    fn drop(&mut self) {
        (self.release)(self.token);
    }
}

/// UI keeps the displayed image leased until a replacement has finished GPU
/// sampling. A staged image that is superseded before presentation returns at once.
#[derive(Default)]
pub(crate) struct PresentationLeases {
    current: Option<Arc<FrameLease>>,
    staged: Option<Option<Arc<FrameLease>>>,
}
impl PresentationLeases {
    pub(crate) fn stage(&mut self, lease: Option<Arc<FrameLease>>) {
        self.staged = Some(lease);
    }
    pub(crate) fn presented(&mut self) {
        if let Some(lease) = self.staged.take() {
            self.current = lease;
        }
    }
    pub(crate) fn clear(&mut self) {
        self.staged = None;
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    fn lease(token: u64, returned: &Arc<Mutex<Vec<u64>>>) -> Arc<FrameLease> {
        let returned = Arc::clone(returned);
        FrameLease::new(token, Arc::new(move |id| returned.lock().unwrap().push(id)))
    }
    #[test]
    fn displayed_frame_survives_staging_and_superseded_frames_return_immediately() {
        let returned = Arc::new(Mutex::new(Vec::new()));
        let mut presentation = PresentationLeases::default();
        presentation.stage(Some(lease(11, &returned)));
        presentation.presented();
        presentation.stage(Some(lease(22, &returned)));
        assert!(
            returned.lock().unwrap().is_empty(),
            "current image is still sampled"
        );
        presentation.stage(Some(lease(33, &returned)));
        assert_eq!(
            *returned.lock().unwrap(),
            [22],
            "only the never-presented frame can return"
        );
        presentation.presented();
        assert_eq!(*returned.lock().unwrap(), [22, 11]);
        presentation.clear();
        assert_eq!(*returned.lock().unwrap(), [22, 11, 33]);
    }
    #[test]
    fn cpu_replacement_waits_for_presentation_before_returning_a_gpu_image() {
        let returned = Arc::new(Mutex::new(Vec::new()));
        let mut presentation = PresentationLeases::default();
        presentation.stage(Some(lease(41, &returned)));
        presentation.presented();
        presentation.stage(None);
        assert!(returned.lock().unwrap().is_empty());
        presentation.presented();
        assert_eq!(*returned.lock().unwrap(), [41]);
    }
    #[test]
    fn cloning_a_frame_lease_cannot_return_storage_while_another_owner_uses_it() {
        let returned = Arc::new(Mutex::new(Vec::new()));
        let frame = lease(51, &returned);
        let display = Arc::clone(&frame);
        drop(frame);
        assert!(returned.lock().unwrap().is_empty());
        drop(display);
        assert_eq!(*returned.lock().unwrap(), [51]);
    }
}
