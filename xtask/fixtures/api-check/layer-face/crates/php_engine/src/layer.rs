mod sealed {
    pub trait Sealed {}
}

pub trait StreamLayer: sealed::Sealed {
    fn host_fd(&self);
}

pub fn poll_fd() {}
