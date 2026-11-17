//! Positional reads from a file or an object (a whole file in memory, a local file, a ranged read of a bucket).

use std::io;

/// Bytes read by offset. A reader checks every range against [`ReadAt::len`] before it reads, so an
/// implementation never sees a range past the end from this crate.
pub trait ReadAt {
    /// The length of the file.
    fn len(&self) -> u64;

    /// Whether the file is empty.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The `len` bytes at `offset`.
    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>>;
}

impl ReadAt for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        usize::try_from(offset)
            .ok()
            .and_then(|o| self.get(o..o.checked_add(len)?))
            .map(<[u8]>::to_vec)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))
    }
}

impl ReadAt for Vec<u8> {
    fn len(&self) -> u64 {
        self.as_slice().len() as u64
    }

    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        self.as_slice().read_at(offset, len)
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn len(&self) -> u64 {
        (**self).len()
    }

    fn read_at(&self, offset: u64, len: usize) -> io::Result<Vec<u8>> {
        (**self).read_at(offset, len)
    }
}
