#![allow(unused)]

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use flate2::read::ZlibDecoder;
use once_cell::unsync::OnceCell;

use std::{
    any::{type_name, Any},
    borrow::Borrow,
    fmt,
    io::{Read, Seek},
    ops::{Deref, DerefMut},
    {
        cell::{Ref, RefCell, RefMut},
        rc::{Rc, Weak},
        string::FromUtf16Error,
    },
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Needed {
    Size(usize),
    Unknown,
}
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum KError {
    Incomplete(Needed),
    EmptyIterator,
    Encoding { desc: String },
    MissingInstanceValue,
    MissingRoot,
    MissingParent,
    ReadBitsTooLarge { requested: usize },
    UnexpectedContents { actual: Vec<u8> },
    ValidationNotEqual(String),
    UnknownVariant(i64),
    EncounteredEOF,
    IoError { desc: String },
    CastError,
    UndecidedEndiannessError(String),
}
pub type KResult<T> = Result<T, KError>;

pub trait CustomDecoder {
    fn decode(&self, bytes: &[u8]) -> Vec<u8>;
}

#[derive(Default)]
pub struct SharedType<T>(RefCell<Weak<T>>);

impl<T> Clone for SharedType<T> {
    fn clone(&self) -> Self {
        Self(RefCell::new(Weak::clone(&*self.0.borrow())))
    }
}

// stop recursion while printing
impl<T> fmt::Debug for SharedType<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let w = &*self.0.borrow();
        match w.strong_count() {
            _ => write!(f, "SharedType(Weak({:?}))", Weak::<T>::as_ptr(w)),
            0 => write!(f, "SharedType(Empty)"),
        }
    }
}

impl<T> SharedType<T> {
    pub fn new(rc: Rc<T>) -> Self {
        Self(RefCell::new(Rc::downgrade(&rc)))
    }

    pub fn empty() -> Self {
        Self(RefCell::new(Weak::new()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.borrow().strong_count() == 0
    }

    pub fn get(&self) -> KResult<OptRc<T>> {
        match self.0.borrow().upgrade() {
            Some(rc) => Ok(OptRc::from(rc)),
            None => Err(KError::MissingParent),
        }
    }

    pub fn get_value(&self) -> &RefCell<Weak<T>> {
        &self.0
    }

    pub fn set(&self, rc: KResult<OptRc<T>>) {
        *self.0.borrow_mut() = match rc.ok() {
            Some(v) => Rc::downgrade(&v.get()),
            None => Weak::new(),
        }
    }
}

// we use own type OptRc<> instead of Rc<> only for one reason:
// by default to not create default value of type T (instead contain Option(None))
// (T could have cyclic-types inside, as a result we got stack overflow)
#[derive(Debug)]
pub struct OptRc<T>(Option<Rc<T>>);

impl<T> OptRc<T> {
    pub fn new(orc: &Option<Rc<T>>) -> Self {
        match orc {
            Some(rc) => OptRc::from(rc.clone()),
            None => OptRc::default(),
        }
    }

    pub fn get(&self) -> Rc<T> {
        self.0.as_ref().unwrap().clone()
    }

    pub fn get_value(&self) -> &Option<Rc<T>> {
        &self.0
    }

    pub fn is_none(&self) -> bool {
        self.0.is_none()
    }

    pub fn get_mut(&mut self) -> &mut Rc<T> {
        self.0.as_mut().unwrap()
    }
}

impl<T> Default for OptRc<T> {
    #[inline]
    fn default() -> Self {
        OptRc(None)
    }
}

impl<T> Clone for OptRc<T> {
    fn clone(&self) -> Self {
        OptRc(self.0.clone())
    }
}

impl<T> From<Rc<T>> for OptRc<T> {
    fn from(v: Rc<T>) -> Self {
        OptRc(Some(v))
    }
}

impl<T> From<T> for OptRc<T> {
    fn from(v: T) -> Self {
        OptRc(Some(v.into()))
    }
}

impl<T> Deref for OptRc<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().unwrap()
    }
}

pub trait KStruct: Default {
    type Root: KStruct;
    type Parent: KStruct;

    /// Parse this struct (and any children) from the supplied stream
    fn read<S: KStream>(
        self_rc: &OptRc<Self>,
        _io: &S,
        _root: SharedType<Self::Root>,
        _parent: SharedType<Self::Parent>,
    ) -> KResult<()>;

    /// helper function to read struct
    fn read_into<S: KStream, T: KStruct + Default + Any>(
        _io: &S,
        _root: Option<SharedType<T::Root>>,
        _parent: Option<SharedType<T::Parent>>,
    ) -> KResult<OptRc<T>> {
        let t = OptRc::from(T::default());
        let root = Self::downcast(_root, t.clone(), true);
        let parent = Self::downcast(_parent, t.clone(), false);
        T::read(&t, _io, root, parent)?;
        Ok(t)
    }

    /// helper function to special initialize and read struct
    fn read_into_with_init<S: KStream, T: KStruct + Default + Any>(
        _io: &S,
        _root: Option<SharedType<T::Root>>,
        _parent: Option<SharedType<T::Parent>>,
        init: &dyn Fn(&mut T) -> KResult<()>,
    ) -> KResult<OptRc<T>> {
        let mut t = OptRc::from(T::default());
        init(Rc::get_mut(t.get_mut()).unwrap())?;

        let root = Self::downcast(_root, t.clone(), true);
        let parent = Self::downcast(_parent, t.clone(), false);
        T::read(&t, _io, root, parent)?;
        Ok(t)
    }

    fn downcast<T, U>(opt_rc: Option<SharedType<U>>, t: OptRc<T>, panic: bool) -> SharedType<U>
    where
        T: KStruct + Default + Any,
        U: 'static,
    {
        if let Some(rc) = opt_rc {
            rc
        } else {
            let t_any = &t.get() as &dyn Any;
            //println!("`{}` is a '{}' type", type_name_of_val(&t), type_name::<Rc<U>>());
            match t_any.downcast_ref::<Rc<U>>() {
                Some(as_result) => SharedType::<U>::new(Rc::clone(as_result)),
                None => {
                    if (panic) {
                        #[cfg(feature = "type_name_of_val")]
                        panic!(
                            "`{}` is not a '{}' type",
                            std::any::type_name_of_val(&t),
                            type_name::<Rc<U>>()
                        );
                        #[cfg(not(feature = "type_name_of_val"))]
                        panic!("`{:p}` is not a '{}' type", &t, type_name::<Rc<U>>());
                    }
                    SharedType::<U>::empty()
                }
            }
        }
    }
}

/// Dummy struct used to indicate an absence of value; needed for
/// root structs to satisfy the associated type bounds in the
/// `KStruct` trait.
#[derive(Debug, Default, Copy, Clone, PartialEq)]
pub struct KStructUnit;

impl KStruct for KStructUnit {
    type Root = KStructUnit;
    type Parent = KStructUnit;

    fn read<S: KStream>(
        _self_rc: &OptRc<Self>,
        _io: &S,
        _root: SharedType<Self::Root>,
        _parent: SharedType<Self::Parent>,
    ) -> KResult<()> {
        Ok(())
    }
}

use std::{fs, path::Path};

impl From<std::io::Error> for KError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError {
            desc: err.to_string(),
        }
    }
}

pub trait KStream {
    fn clone(&self) -> BytesReader;
    fn size(&self) -> usize;

    fn is_eof(&self) -> bool {
        if self.get_state().bits_left > 0 {
            return false;
        }
        self.pos() == self.size()
    }

    fn seek(&self, position: usize) -> KResult<()> {
        if position > self.size() {
            return Err(KError::Incomplete(Needed::Size(position - self.pos())));
        }
        self.get_state_mut().pos = position;
        Ok(())
    }

    fn pos(&self) -> usize {
        self.get_state().pos
    }

    fn read_s1(&self) -> KResult<i8> {
        Ok(self.read_bytes(1)?[0] as i8)
    }
    fn read_s2be(&self) -> KResult<i16> {
        Ok(BigEndian::read_i16(&self.read_bytes(2)?))
    }
    fn read_s4be(&self) -> KResult<i32> {
        Ok(BigEndian::read_i32(&self.read_bytes(4)?))
    }
    fn read_s8be(&self) -> KResult<i64> {
        Ok(BigEndian::read_i64(&self.read_bytes(8)?))
    }
    fn read_s2le(&self) -> KResult<i16> {
        Ok(LittleEndian::read_i16(&self.read_bytes(2)?))
    }
    fn read_s4le(&self) -> KResult<i32> {
        Ok(LittleEndian::read_i32(&self.read_bytes(4)?))
    }
    fn read_s8le(&self) -> KResult<i64> {
        Ok(LittleEndian::read_i64(&self.read_bytes(8)?))
    }

    fn read_u1(&self) -> KResult<u8> {
        Ok(self.read_bytes(1)?[0])
    }
    fn read_u2be(&self) -> KResult<u16> {
        Ok(BigEndian::read_u16(&self.read_bytes(2)?))
    }
    fn read_u4be(&self) -> KResult<u32> {
        Ok(BigEndian::read_u32(&self.read_bytes(4)?))
    }
    fn read_u8be(&self) -> KResult<u64> {
        Ok(BigEndian::read_u64(&self.read_bytes(8)?))
    }
    fn read_u2le(&self) -> KResult<u16> {
        Ok(LittleEndian::read_u16(&self.read_bytes(2)?))
    }
    fn read_u4le(&self) -> KResult<u32> {
        Ok(LittleEndian::read_u32(&self.read_bytes(4)?))
    }
    fn read_u8le(&self) -> KResult<u64> {
        Ok(LittleEndian::read_u64(&self.read_bytes(8)?))
    }

    fn read_f4be(&self) -> KResult<f32> {
        Ok(BigEndian::read_f32(&self.read_bytes(4)?))
    }
    fn read_f8be(&self) -> KResult<f64> {
        Ok(BigEndian::read_f64(&self.read_bytes(8)?))
    }
    fn read_f4le(&self) -> KResult<f32> {
        Ok(LittleEndian::read_f32(&self.read_bytes(4)?))
    }
    fn read_f8le(&self) -> KResult<f64> {
        Ok(LittleEndian::read_f64(&self.read_bytes(8)?))
    }

    fn get_state(&self) -> Ref<ReaderState>;
    fn get_state_mut(&self) -> RefMut<ReaderState>;

    fn align_to_byte(&self) -> KResult<()> {
        let mut inner = self.get_state_mut();
        inner.bits = 0;
        inner.bits_left = 0;

        Ok(())
    }

    fn read_bits_int_be(&self, n: usize) -> KResult<u64> {
        let mut res: u64 = 0;

        if n > 64 {
            return Err(KError::ReadBitsTooLarge { requested: n });
        }

        let n = n as i64;
        let bits_needed = n - self.get_state().bits_left;
        self.get_state_mut().bits_left = -bits_needed & 7;

        if bits_needed > 0 {
            let bytes_needed = ((bits_needed - 1) / 8) + 1;
            let buf = self.read_bytes(bytes_needed as usize)?;
            for b in buf {
                res = res << 8 | (b as u64);
            }
            let mut inner = self.get_state_mut();
            let new_bits = res;
            res >>= inner.bits_left;
            if bits_needed < 64 {
                res |= inner.bits << bits_needed;
            }
            inner.bits = new_bits;
        } else {
            res = self.get_state().bits >> -bits_needed;
        }

        let mut inner = self.get_state_mut();
        let mut mask = (1u64 << inner.bits_left) - 1;
        inner.bits &= mask;

        Ok(res)
    }

    fn read_bits_int_le(&self, n: usize) -> KResult<u64> {
        let mut res: u64 = 0;

        if n > 64 {
            return Err(KError::ReadBitsTooLarge { requested: n });
        }

        let n = n as i64;
        let bits_needed = n - self.get_state().bits_left;

        if bits_needed > 0 {
            let bytes_needed = ((bits_needed - 1) / 8) + 1;
            let buf = self.read_bytes(bytes_needed as usize)?;
            for i in 0..bytes_needed {
                res |= (buf[i as usize] as u64) << (i * 8);
            }
            let mut inner = self.get_state_mut();
            let new_bits = if bits_needed < 64 {
                res >> bits_needed
            } else {
                0
            };
            res = res << inner.bits_left | inner.bits;
            inner.bits = new_bits;
        } else {
            let mut inner = self.get_state_mut();
            res = inner.bits;
            inner.bits >>= n;
        }

        let mut inner = self.get_state_mut();
        inner.bits_left = -bits_needed & 7;

        if n < 64 {
            let mut mask = (1u64 << n) - 1;
            res &= mask;
        }

        Ok(res)
    }

    fn read_bytes(&self, len: usize) -> KResult<Vec<u8>>;
    fn read_bytes_full(&self) -> KResult<Vec<u8>>;

    fn read_bytes_term(
        &self,
        term: u8,
        include: bool,
        consume: bool,
        eos_error: bool,
    ) -> KResult<Vec<u8>> {
        let mut readed_bytes: Vec<u8> = Vec::new();
        while self.pos() < self.size() {
            let bytes = self.read_bytes(1)?;
            if bytes[0] != term {
                readed_bytes.push(bytes[0]);
            } else {
                // undo last readed byte
                self.get_state_mut().pos -= 1;
                break;
            }
        }

        if self.pos() == self.size() {
            if eos_error {
                return Err(KError::EncounteredEOF);
            }
            Ok(readed_bytes)
        } else {
            // consume terminator?
            if consume {
                self.get_state_mut().pos += 1;
            }
            // but return or not 'term' symbol depend on 'include' flag
            if include {
                readed_bytes.push(term);
            }
            Ok(readed_bytes)
        }
    }

    /// Return a byte array that is sized to exclude all trailing instances of the
    /// padding character.
    fn bytes_strip_right(bytes: &Vec<u8>, pad: u8) -> Vec<u8> {
        let mut new_len = bytes.len();
        while new_len > 0 && bytes[new_len - 1] == pad {
            new_len -= 1;
        }
        bytes[..new_len].to_vec()
    }

    /// Return a byte array that contains all bytes up until the
    /// termination byte. Can optionally include the termination byte as well.
    fn bytes_terminate(bytes: &Vec<u8>, term: u8, include_term: bool) -> Vec<u8> {
        let mut new_len = 0;
        while bytes[new_len] != term && new_len < bytes.len() {
            new_len += 1;
        }

        if include_term && new_len < bytes.len() {
            new_len += 1;
        }

        bytes[..new_len].to_vec()
    }

    fn process_xor_one(bytes: &Vec<u8>, key: u8) -> Vec<u8> {
        let mut res = bytes.to_vec();
        for i in res.iter_mut() {
            *i ^= key;
        }
        res
    }

    fn process_xor_many(bytes: &Vec<u8>, key: &[u8]) -> Vec<u8> {
        let mut res = bytes.to_vec();
        let mut ki = 0;
        for i in res.iter_mut() {
            *i ^= key[ki];
            ki += 1;
            if (ki >= key.len()) {
                ki = 0;
            }
        }
        res
    }

    fn process_rotate_left(bytes: &Vec<u8>, amount: u8) -> Vec<u8> {
        let mut res = bytes.to_vec();
        for i in res.iter_mut() {
            *i = (*i << amount) | (*i >> (8 - amount));
        }
        res
    }

    fn process_zlib(bytes: &Vec<u8>) -> Vec<u8> {
        let mut dec = ZlibDecoder::new(bytes.as_slice());
        let mut dec_bytes = Vec::new();
        dec.read_to_end(&mut dec_bytes);
        dec_bytes
    }
}

#[derive(Default, Debug, Clone)]
pub struct ReaderState {
    pos: usize,
    bits: u64,
    bits_left: i64,
}

trait ReadSeek: Read + Seek {}

impl<T> ReadSeek for T where T: Read + Seek {}

impl fmt::Display for dyn ReadSeek {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReadSeek")
    }
}

impl fmt::Debug for dyn ReadSeek {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ReadSeek")
    }
}

#[derive(Debug, Default, Clone)]
pub struct BytesReader {
    state: RefCell<ReaderState>,
    // share same "instance" of data beetween all clones
    // reposition before each read call
    buf: OptRc<RefCell<Box<dyn ReadSeek>>>,
    file_size: u64,
}

impl From<Vec<u8>> for BytesReader {
    fn from(bytes: Vec<u8>) -> BytesReader {
        BytesReader::from_buffer(&bytes)
    }
}

impl From<&'static [u8]> for BytesReader {
    fn from(slice: &'static [u8]) -> BytesReader {
        BytesReader::from_buffer(slice)
    }
}

impl BytesReader {
    pub fn open<T: AsRef<Path>>(filename: T) -> KResult<Self> {
        let f = std::fs::File::open(filename).map_err(|e| KError::IoError {
            desc: e.to_string(),
        })?;
        let file_size = f.metadata().unwrap().len();
        let r: Box<dyn ReadSeek> = Box::new(f);
        Ok(BytesReader {
            state: RefCell::new(ReaderState::default()),
            file_size,
            buf: OptRc::from(RefCell::new(r)),
        })
    }

    fn from_buffer(bytes: &[u8]) -> Self {
        let file_size = bytes.len() as u64;
        let r: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(bytes.to_vec()));
        BytesReader {
            state: RefCell::new(ReaderState::default()),
            file_size,
            buf: OptRc::from(RefCell::new(r)),
        }
    }

    // sync stream pos with state.pos
    fn sync_pos(&self) -> KResult<()> {
        let cur_pos = self
            .buf
            .borrow_mut()
            .stream_position()
            .map_err(|e| KError::IoError {
                desc: e.to_string(),
            })?;
        if self.pos() != cur_pos as usize {
            self.buf
                .borrow_mut()
                .seek(SeekFrom::Start(self.pos() as u64))
                .map_err(|e| KError::IoError {
                    desc: e.to_string(),
                })?;
        }
        Ok(())
    }
}

use std::io::BufRead;
use std::io::SeekFrom;

impl KStream for BytesReader {
    fn clone(&self) -> Self {
        Clone::clone(self)
    }

    fn get_state(&self) -> Ref<ReaderState> {
        self.state.borrow()
    }

    fn get_state_mut(&self) -> RefMut<ReaderState> {
        self.state.borrow_mut()
    }

    fn size(&self) -> usize {
        self.file_size as usize
    }

    fn read_bytes(&self, len: usize) -> KResult<Vec<u8>> {
        // handle read beyond end of file
        if len + self.pos() > self.size() {
            return Err(KError::Incomplete(Needed::Size(
                len + self.pos() - self.size(),
            )));
        }
        self.sync_pos()?;
        // let state = self.state.borrow_mut();
        // state.buf.resize(len, 0);
        let mut buf = vec![0; len];
        let readed = self
            .buf
            .borrow_mut()
            .read(&mut buf[..])
            .map_err(|e| KError::IoError {
                desc: e.to_string(),
            })?;
        self.get_state_mut().pos += readed;
        Ok(buf)
    }

    fn read_bytes_full(&self) -> KResult<Vec<u8>> {
        self.sync_pos()?;
        //let state = self.state.borrow_mut();
        let mut buf = Vec::new();
        let readed = self
            .buf
            .borrow_mut()
            .read_to_end(&mut buf)
            .map_err(|e| KError::IoError {
                desc: e.to_string(),
            })?;
        self.get_state_mut().pos += readed;
        Ok(buf)
    }
}

use encoding::label::encoding_from_whatwg_label;
use encoding::{DecoderTrap, Encoding};

pub fn decode_string(bytes: &Vec<u8>, label: &str) -> KResult<String> {
    if let Some(enc) = encoding_from_whatwg_label(label) {
        return enc
            .decode(bytes.as_slice(), DecoderTrap::Replace)
            .map_err(|e| KError::Encoding {
                desc: e.to_string(),
            });
    }

    let enc = label.to_lowercase();
    if enc == "cp437" || enc == "ibm437" {
        use std::io::BufReader;
        let reader = BufReader::new(bytes.as_slice());
        let mut buffer = reader.bytes();
        let mut r = cp437::Reader::new(&mut buffer);
        return Ok(r.consume(bytes.len()));
    }

    Err(KError::Encoding {
        desc: format!("decode_string: unknown WHATWG Encoding standard: {}", label),
    })
}

pub fn reverse_string<S: AsRef<str>>(s: S) -> KResult<String> {
    Ok(s.as_ref().to_string().graphemes(true).rev().collect())
}

pub fn modulo(a: i64, b: i64) -> i64 {
    let mut r = a % b;
    if r < 0 {
        r += b;
    }
    r
}

macro_rules! kf_max {
    ($i: ident, $t: ty) => {
        pub fn $i<'a>(first: Option<&'a $t>, second: &'a $t) -> Option<&'a $t> {
            if second.is_nan() {
                first
            } else if first.is_none() {
                Some(second)
            } else {
                if first.unwrap() < second {
                    Some(second)
                } else {
                    first
                }
            }
        }
    };
}
kf_max!(kf32_max, f32);
kf_max!(kf64_max, f64);

macro_rules! kf_min {
    ($i: ident, $t: ty) => {
        pub fn $i<'a>(first: Option<&'a $t>, second: &'a $t) -> Option<&'a $t> {
            if second.is_nan() {
                first
            } else if first.is_none() {
                Some(second)
            } else {
                if first.unwrap() < second {
                    first
                } else {
                    Some(second)
                }
            }
        }
    };
}
kf_min!(kf32_min, f32);
kf_min!(kf64_min, f64);

#[cfg(test)]
mod tests {
    use std::{borrow::BorrowMut, clone, io::Write};

    use super::*;

    #[test]
    fn basic_strip_right() {
        let b = vec![1, 2, 3, 4, 5, 5, 5, 5];
        let reader = BytesReader::from(vec![]);
        let c = BytesReader::bytes_strip_right(&b, 5);

        assert_eq!([1, 2, 3, 4], c[..]);
    }

    #[test]
    fn basic_read_bytes() {
        let b = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let reader = BytesReader::from(b);

        assert_eq!(reader.read_bytes(4).unwrap()[..], [1, 2, 3, 4]);
        assert_eq!(reader.read_bytes(3).unwrap()[..], [5, 6, 7]);
        assert_eq!(
            reader.read_bytes(4).unwrap_err(),
            KError::Incomplete(Needed::Size(3))
        );
        assert_eq!(reader.read_bytes(1).unwrap()[..], [8]);
    }

    #[test]
    fn read_bits_single() {
        let b = vec![0x80];
        let reader = BytesReader::from(b);

        assert_eq!(reader.read_bits_int_be(1).unwrap(), 1);
    }

    #[test]
    fn read_bits_multiple() {
        // 0xA0
        let b = vec![0b10100000];
        let reader = BytesReader::from(b);

        assert_eq!(reader.read_bits_int_be(1).unwrap(), 1);
        assert_eq!(reader.read_bits_int_be(1).unwrap(), 0);
        assert_eq!(reader.read_bits_int_be(1).unwrap(), 1);
    }

    #[test]
    fn read_bits_large() {
        let b = vec![0b10100000];
        let reader = BytesReader::from(b);

        assert_eq!(reader.read_bits_int_be(3).unwrap(), 5);
    }

    #[test]
    fn read_bits_span() {
        let b = vec![0x01, 0x80];
        let reader = BytesReader::from(b);

        assert_eq!(reader.read_bits_int_be(9).unwrap(), 3);
    }

    #[test]
    fn read_bits_too_large() {
        let b: Vec<u8> = vec![1, 2, 3, 4, 5, 6, 7, 8, 9];
        let reader = BytesReader::from(b);

        assert_eq!(
            reader.read_bits_int_be(65).unwrap_err(),
            KError::ReadBitsTooLarge { requested: 65 }
        )
    }

    #[test]
    fn read_bytes_term() {
        let b = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10];
        let reader = BytesReader::from(b);

        assert_eq!(
            reader.read_bytes_term(3, false, false, false).unwrap()[..],
            [1, 2]
        );
        assert_eq!(
            reader.read_bytes_term(3, true, false, true).unwrap()[..],
            [3]
        );
        assert_eq!(
            reader.read_bytes_term(3, false, true, true).unwrap()[..],
            []
        );
        assert_eq!(
            reader.read_bytes_term(5, true, true, true).unwrap()[..],
            [4, 5]
        );
        assert_eq!(
            reader.read_bytes_term(8, false, false, true).unwrap()[..],
            [6, 7]
        );
        assert_eq!(
            reader.read_bytes_term(11, false, true, true).unwrap_err(),
            KError::EncounteredEOF
        );
        // restore position
        reader.seek(7);
        assert_eq!(
            reader.read_bytes_term(9, true, true, false).unwrap()[..],
            [8, 9]
        );
        assert_eq!(
            reader.read_bytes_term(10, true, false, false).unwrap()[..],
            [10]
        );
    }

    #[test]
    fn process_xor_one_test() {
        let b = vec![0x66];
        let reader = BytesReader::from(b);
        let res = BytesReader::process_xor_one(&reader.read_bytes(1).unwrap(), 3);
        assert_eq!(0x65, res[0]);
    }

    #[test]
    fn process_xor_many_test() {
        let b = vec![0x66, 0x6F];
        let reader = BytesReader::from(b);
        let key: Vec<u8> = vec![3, 3];
        let res = BytesReader::process_xor_many(&reader.read_bytes(2).unwrap(), &key);
        assert_eq!(vec![0x65, 0x6C], res);
    }

    #[test]
    fn process_rotate_left_test() {
        let b = vec![0x09, 0xAC];
        let reader = BytesReader::from(b);
        let res = BytesReader::process_rotate_left(&reader.read_bytes(2).unwrap(), 3);
        let expected: Vec<u8> = vec![0x48, 0x65];
        assert_eq!(expected, res);
    }

    #[test]
    fn basic_seek() {
        let b = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let reader = BytesReader::from(b);

        assert_eq!(reader.read_bytes(4).unwrap()[..], [1, 2, 3, 4]);
        let pos = reader.pos();
        reader.seek(1).unwrap();
        assert_eq!(reader.read_bytes(4).unwrap()[..], [2, 3, 4, 5]);
        reader.seek(pos).unwrap();
        assert_eq!(reader.read_bytes(4).unwrap()[..], [5, 6, 7, 8]);
        assert_eq!(
            reader.seek(9).unwrap_err(),
            KError::Incomplete(Needed::Size(1))
        );
    }

    use tempfile::tempdir;

    fn dump_and_open(bytes: &[u8]) -> BytesReader {
        let mut tmp_dir = tempdir().unwrap();
        let file_path = tmp_dir.path().join("test.txt");
        {
            let mut tmp_file = std::fs::File::create(file_path.clone()).unwrap();
            tmp_file.write_all(bytes);
        }
        BytesReader::open(file_path).unwrap()
    }

    #[test]
    fn basic_read_bytes_file() {
        let reader = dump_and_open(&vec![1, 2, 3, 4, 5, 6, 7, 8]);

        assert_eq!(reader.read_bytes(4).unwrap()[..], [1, 2, 3, 4]);
        assert_eq!(reader.read_bytes(3).unwrap()[..], [5, 6, 7]);
        assert_eq!(
            reader.read_bytes(4).unwrap_err(),
            KError::Incomplete(Needed::Size(3))
        );
        assert_eq!(reader.read_bytes(1).unwrap()[..], [8]);
    }

    #[test]
    fn basic_seek_file() {
        let reader = dump_and_open(&vec![1, 2, 3, 4, 5, 6, 7, 8]);

        assert_eq!(reader.read_bytes(4).unwrap()[..], [1, 2, 3, 4]);
        let pos = reader.pos();
        reader.seek(1).unwrap();
        assert_eq!(reader.read_bytes(4).unwrap()[..], [2, 3, 4, 5]);
        reader.seek(pos).unwrap();
        assert_eq!(reader.read_bytes(4).unwrap()[..], [5, 6, 7, 8]);
        assert_eq!(
            reader.seek(9).unwrap_err(),
            KError::Incomplete(Needed::Size(1))
        );
    }
}
