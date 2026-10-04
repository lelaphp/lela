//! The socket and fd group of the native face (design §4.4,
//! §4.3 seam 5): the system calls `main/network.c` and the plain files
//! kind make over the fds a stream holds, `php_heap`'s bindings in its
//! `unsafe` home, which the stream layer above the engine reaches here,
//! having no edge of its own to the heap (#1874).

pub use php_heap::{
    BUFSIZ, Readiness, SockOpt, bind_fd, connect_fd, dup_fd, fd_table_size, listen_fd, new_socket,
    peek_fd, poll_fd, set_fd_blocking, set_sockopt, sync_fd,
};
