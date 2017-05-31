use super::{huffman, Entry, Key};
use util::byte_str::FromUtf8Error;

use http::{method, header, status, StatusCode, Method};
use bytes::{Buf, Bytes};

use std::cmp;
use std::io::Cursor;
use std::collections::VecDeque;

/// Decodes headers using HPACK
pub struct Decoder {
    // Protocol indicated that the max table size will update
    max_size_update: Option<usize>,
    table: Table,
}

/// Represents all errors that can be encountered while performing the decoding
/// of an HPACK header set.
#[derive(Debug, Copy, Clone, PartialEq)]
pub enum DecoderError {
    InvalidRepresentation,
    InvalidIntegerPrefix,
    InvalidTableIndex,
    InvalidHuffmanCode,
    InvalidUtf8,
    InvalidStatusCode,
    InvalidPseudoheader,
    InvalidMaxDynamicSize,
    IntegerUnderflow,
    IntegerOverflow,
    StringUnderflow,
}

enum Representation {
    /// Indexed header field representation
    ///
    /// An indexed header field representation identifies an entry in either the
    /// static table or the dynamic table (see Section 2.3).
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 1 |        Index (7+)         |
    /// +---+---------------------------+
    /// ```
    Indexed,

    /// Literal Header Field with Incremental Indexing
    ///
    /// A literal header field with incremental indexing representation results
    /// in appending a header field to the decoded header list and inserting it
    /// as a new entry into the dynamic table.
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 1 |      Index (6+)       |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    LiteralWithIndexing,

    /// Literal Header Field without Indexing
    ///
    /// A literal header field without indexing representation results in
    /// appending a header field to the decoded header list without altering the
    /// dynamic table.
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 0 | 0 |  Index (4+)   |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    LiteralWithoutIndexing,

    /// Literal Header Field Never Indexed
    ///
    /// A literal header field never-indexed representation results in appending
    /// a header field to the decoded header list without altering the dynamic
    /// table. Intermediaries MUST use the same representation for encoding this
    /// header field.
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 0 | 1 |  Index (4+)   |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    LiteralNeverIndexed,

    /// Dynamic Table Size Update
    ///
    /// A dynamic table size update signals a change to the size of the dynamic
    /// table.
    ///
    /// # Header encoding
    ///
    /// ```text
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 1 |   Max size (5+)   |
    /// +---+---------------------------+
    /// ```
    SizeUpdate,
}

struct Table {
    entries: VecDeque<Entry>,
    size: usize,
    max_size: usize,
}

// ===== impl Decoder =====

impl Decoder {
    /// Creates a new `Decoder` with all settings set to default values.
    pub fn new(size: usize) -> Decoder {
        Decoder {
            max_size_update: None,
            table: Table::new(size),
        }
    }

    /// Queues a potential size update
    pub fn queue_size_update(&mut self, size: usize) {
        let size = match self.max_size_update {
            Some(v) => cmp::min(v, size),
            None => size,
        };

        self.max_size_update = Some(size);
    }

    /// Decodes the headers found in the given buffer.
    pub fn decode<F>(&mut self, src: &Bytes, mut f: F) -> Result<(), DecoderError>
        where F: FnMut(Entry)
    {
        use self::Representation::*;

        let mut buf = Cursor::new(src);
        let mut can_resize = true;

        while buf.has_remaining() {
            // At this point we are always at the beginning of the next block
            // within the HPACK data. The type of the block can always be
            // determined from the first byte.
            match try!(Representation::load(peek_u8(&mut buf))) {
                Indexed => {
                    can_resize = false;
                    f(try!(self.decode_indexed(&mut buf)));
                }
                LiteralWithIndexing => {
                    can_resize = false;
                    let entry = try!(self.decode_literal(&mut buf, true));

                    // Insert the header into the table
                    self.table.insert(entry.clone());

                    f(entry);
                }
                LiteralWithoutIndexing => {
                    can_resize = false;
                    let entry = try!(self.decode_literal(&mut buf, false));
                    f(entry);
                }
                LiteralNeverIndexed => {
                    can_resize = false;
                    let entry = try!(self.decode_literal(&mut buf, false));

                    // TODO: Track that this should never be indexed

                    f(entry);
                }
                SizeUpdate => {
                    let max = match self.max_size_update.take() {
                        Some(max) if can_resize => max,
                        _ => {
                            // Resize is too big or other frames have been read
                            // before the resize.
                            return Err(DecoderError::InvalidMaxDynamicSize);
                        }
                    };

                    // Handle the dynamic table size update...
                    try!(self.process_size_update(&mut buf, max))
                }
            }
        }

        Ok(())
    }

    fn process_size_update(&mut self, buf: &mut Cursor<&Bytes>, max: usize)
        -> Result<(), DecoderError>
    {
        let new_size = try!(decode_int(buf, 5));

        if new_size > max {
            return Err(DecoderError::InvalidMaxDynamicSize);
        }

        debug!("Decoder changed max table size from {} to {}",
               self.table.size(), new_size);

        self.table.set_max_size(new_size);

        Ok(())
    }

    fn decode_indexed(&self, buf: &mut Cursor<&Bytes>)
        -> Result<Entry, DecoderError>
    {
        let index = try!(decode_int(buf, 7));
        self.table.get(index)
    }

    fn decode_literal(&self, buf: &mut Cursor<&Bytes>, index: bool)
        -> Result<Entry, DecoderError>
    {
        let prefix = if index {
            6
        } else {
            4
        };

        // Extract the table index for the name, or 0 if not indexed
        let table_idx = try!(decode_int(buf, prefix));

        // First, read the header name
        if table_idx == 0 {
            // Read the name as a literal
            let name = try!(decode_string(buf));
            let value = try!(decode_string(buf));

            Entry::new(name, value)
        } else {
            let e = try!(self.table.get(table_idx));
            let value = try!(decode_string(buf));

            e.key().into_entry(value)
        }
    }
}

impl Default for Decoder {
    fn default() -> Decoder {
        Decoder::new(4096)
    }
}

// ===== impl Representation =====

impl Representation {
    pub fn load(byte: u8) -> Result<Representation, DecoderError> {
        const INDEXED: u8                  = 0b10000000;
        const LITERAL_WITH_INDEXING: u8    = 0b01000000;
        const LITERAL_WITHOUT_INDEXING: u8 = 0b11110000;
        const LITERAL_NEVER_INDEXED: u8    = 0b00010000;
        const SIZE_UPDATE_MASK: u8         = 0b11100000;
        const SIZE_UPDATE: u8              = 0b00100000;

        // TODO: What did I even write here?

        if byte & INDEXED == INDEXED {
            Ok(Representation::Indexed)
        } else if byte & LITERAL_WITH_INDEXING == LITERAL_WITH_INDEXING {
            Ok(Representation::LiteralWithIndexing)
        } else if byte & LITERAL_WITHOUT_INDEXING == 0 {
            Ok(Representation::LiteralWithoutIndexing)
        } else if byte & LITERAL_WITHOUT_INDEXING == LITERAL_NEVER_INDEXED {
            Ok(Representation::LiteralNeverIndexed)
        } else if byte & SIZE_UPDATE_MASK == SIZE_UPDATE {
            Ok(Representation::SizeUpdate)
        } else {
            Err(DecoderError::InvalidRepresentation)
        }
    }
}

fn decode_int<B: Buf>(buf: &mut B, prefix_size: u8) -> Result<usize, DecoderError> {
    // The octet limit is chosen such that the maximum allowed *value* can
    // never overflow an unsigned 32-bit integer. The maximum value of any
    // integer that can be encoded with 5 octets is ~2^28
    const MAX_BYTES: usize = 5;
    const VARINT_MASK: u8 = 0b01111111;
    const VARINT_FLAG: u8 = 0b10000000;

    if prefix_size < 1 || prefix_size > 8 {
        return Err(DecoderError::InvalidIntegerPrefix);
    }

    if !buf.has_remaining() {
        return Err(DecoderError::IntegerUnderflow);
    }

    let mask = if prefix_size == 8 {
        0xFF
    } else {
        (1u8 << prefix_size).wrapping_sub(1)
    };

    let mut ret = (buf.get_u8() & mask) as usize;

    if ret < mask as usize {
        // Value fits in the prefix bits
        return Ok(ret);
    }

    // The int did not fit in the prefix bits, so continue reading.
    //
    // The total number of bytes used to represent the int. The first byte was
    // the prefix, so start at 1.
    let mut bytes = 1;

    // The rest of the int is stored as a varint -- 7 bits for the value and 1
    // bit to indicate if it is the last byte.
    let mut shift = 0;

    while buf.has_remaining() {
        let b = buf.get_u8();

        bytes += 1;
        ret += ((b & VARINT_MASK) as usize) << shift;
        shift += 7;

        if b & VARINT_FLAG == 0 {
            return Ok(ret);
        }

        if bytes == MAX_BYTES {
            // The spec requires that this situation is an error
            return Err(DecoderError::IntegerOverflow);
        }
    }

    Err(DecoderError::IntegerUnderflow)
}

fn decode_string(buf: &mut Cursor<&Bytes>) -> Result<Bytes, DecoderError> {
    const HUFF_FLAG: u8 = 0b10000000;

    // The first bit in the first byte contains the huffman encoded flag.
    let huff = peek_u8(buf) & HUFF_FLAG == HUFF_FLAG;

    // Decode the string length using 7 bit prefix
    let len = try!(decode_int(buf, 7));

    if len > buf.remaining() {
        return Err(DecoderError::StringUnderflow);
    }

    if huff {
        let ret = {
            let raw = &buf.bytes()[..len];
            huffman::decode(raw).map(Into::into)
        };

        buf.advance(len);
        return ret;
    } else {
        Ok(take(buf, len))
    }
}

fn peek_u8<B: Buf>(buf: &mut B) -> u8 {
    buf.bytes()[0]
}

fn take(buf: &mut Cursor<&Bytes>, n: usize) -> Bytes {
    let pos = buf.position() as usize;
    let ret = buf.get_ref().slice(pos, pos + n);
    buf.set_position((pos + n) as u64);
    ret
}

// ===== impl Table =====

impl Table {
    fn new(max_size: usize) -> Table {
        Table {
            entries: VecDeque::new(),
            size: 0,
            max_size: max_size,
        }
    }

    fn max_size(&self) -> usize {
        self.max_size
    }

    fn size(&self) -> usize {
        self.size
    }

    /// Returns the entry located at the given index.
    ///
    /// The table is 1-indexed and constructed in such a way that the first
    /// entries belong to the static table, followed by entries in the dynamic
    /// table. They are merged into a single index address space, though.
    ///
    /// This is according to the [HPACK spec, section 2.3.3.]
    /// (http://http2.github.io/http2-spec/compression.html#index.address.space)
    pub fn get(&self, index: usize) -> Result<Entry, DecoderError> {
        if index == 0 {
            return Err(DecoderError::InvalidTableIndex);
        }

        if index <= 61 {
            return Ok(get_static(index));
        }

        // Convert the index for lookup in the entries structure.
        match self.entries.get(index - 62) {
            Some(e) => Ok(e.clone()),
            None => Err(DecoderError::InvalidTableIndex),
        }
    }

    fn insert(&mut self, entry: Entry) {
        let len = entry.len();

        self.reserve(len);

        self.size += len;

        // Track the entry
        self.entries.push_front(entry);
    }

    fn set_max_size(&mut self, size: usize) {
        self.max_size = size;
        // Make the table size fit within the new constraints.
        self.consolidate();
    }

    fn reserve(&mut self, size: usize) {
        debug_assert!(size <= self.max_size);

        while self.size + size > self.max_size {
            let last = self.entries.pop_back()
                .expect("size of table != 0, but no headers left!");

            self.size -= last.len();
        }
    }

    fn consolidate(&mut self) {
        while self.size > self.max_size {
            {
                let last = match self.entries.back() {
                    Some(x) => x,
                    None => {
                        // Can never happen as the size of the table must reach
                        // 0 by the time we've exhausted all elements.
                        panic!("Size of table != 0, but no headers left!");
                    }
                };

                self.size -= last.len();
            }

            self.entries.pop_back();
        }
    }
}

// ===== impl DecoderError =====

impl From<FromUtf8Error> for DecoderError {
    fn from(src: FromUtf8Error) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

impl From<header::InvalidValueError> for DecoderError {
    fn from(src: header::InvalidValueError) -> DecoderError {
        // TODO: Better error?
        DecoderError::InvalidUtf8
    }
}

impl From<method::FromBytesError> for DecoderError {
    fn from(src: method::FromBytesError) -> DecoderError {
        // TODO: Better error
        DecoderError::InvalidUtf8
    }
}

impl From<header::FromBytesError> for DecoderError {
    fn from(src: header::FromBytesError) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

impl From<status::FromStrError> for DecoderError {
    fn from(src: status::FromStrError) -> DecoderError {
        DecoderError::InvalidUtf8
    }
}

/// Get an entry from the static table
pub fn get_static(idx: usize) -> Entry {
    use http::{status, method, header};
    use http::header::HeaderValue;
    use util::byte_str::ByteStr;

    match idx {
        1 => Entry::Authority(ByteStr::from_static("")),
        2 => Entry::Method(method::GET),
        3 => Entry::Method(method::POST),
        4 => Entry::Path(ByteStr::from_static("/")),
        5 => Entry::Path(ByteStr::from_static("/index.html")),
        6 => Entry::Scheme(ByteStr::from_static("http")),
        7 => Entry::Scheme(ByteStr::from_static("https")),
        8 => Entry::Status(status::OK),
        9 => Entry::Status(status::NO_CONTENT),
        10 => Entry::Status(status::PARTIAL_CONTENT),
        11 => Entry::Status(status::NOT_MODIFIED),
        12 => Entry::Status(status::BAD_REQUEST),
        13 => Entry::Status(status::NOT_FOUND),
        14 => Entry::Status(status::INTERNAL_SERVER_ERROR),
        15 => Entry::Header {
            name: header::ACCEPT_CHARSET,
            value: HeaderValue::from_static(""),
        },
        16 => Entry::Header {
            name: header::ACCEPT_ENCODING,
            value: HeaderValue::from_static("gzip, deflate"),
        },
        17 => Entry::Header {
            name: header::ACCEPT_LANGUAGE,
            value: HeaderValue::from_static(""),
        },
        18 => Entry::Header {
            name: header::ACCEPT_RANGES,
            value: HeaderValue::from_static(""),
        },
        19 => Entry::Header {
            name: header::ACCEPT,
            value: HeaderValue::from_static(""),
        },
        20 => Entry::Header {
            name: header::ACCESS_CONTROL_ALLOW_ORIGIN,
            value: HeaderValue::from_static(""),
        },
        21 => Entry::Header {
            name: header::AGE,
            value: HeaderValue::from_static(""),
        },
        22 => Entry::Header {
            name: header::ALLOW,
            value: HeaderValue::from_static(""),
        },
        23 => Entry::Header {
            name: header::AUTHORIZATION,
            value: HeaderValue::from_static(""),
        },
        24 => Entry::Header {
            name: header::CACHE_CONTROL,
            value: HeaderValue::from_static(""),
        },
        25 => Entry::Header {
            name: header::CONTENT_DISPOSITION,
            value: HeaderValue::from_static(""),
        },
        26 => Entry::Header {
            name: header::CONTENT_ENCODING,
            value: HeaderValue::from_static(""),
        },
        27 => Entry::Header {
            name: header::CONTENT_LANGUAGE,
            value: HeaderValue::from_static(""),
        },
        28 => Entry::Header {
            name: header::CONTENT_LENGTH,
            value: HeaderValue::from_static(""),
        },
        29 => Entry::Header {
            name: header::CONTENT_LOCATION,
            value: HeaderValue::from_static(""),
        },
        30 => Entry::Header {
            name: header::CONTENT_RANGE,
            value: HeaderValue::from_static(""),
        },
        31 => Entry::Header {
            name: header::CONTENT_TYPE,
            value: HeaderValue::from_static(""),
        },
        32 => Entry::Header {
            name: header::COOKIE,
            value: HeaderValue::from_static(""),
        },
        33 => Entry::Header {
            name: header::DATE,
            value: HeaderValue::from_static(""),
        },
        34 => Entry::Header {
            name: header::ETAG,
            value: HeaderValue::from_static(""),
        },
        35 => Entry::Header {
            name: header::EXPECT,
            value: HeaderValue::from_static(""),
        },
        36 => Entry::Header {
            name: header::EXPIRES,
            value: HeaderValue::from_static(""),
        },
        37 => Entry::Header {
            name: header::FROM,
            value: HeaderValue::from_static(""),
        },
        38 => Entry::Header {
            name: header::HOST,
            value: HeaderValue::from_static(""),
        },
        39 => Entry::Header {
            name: header::IF_MATCH,
            value: HeaderValue::from_static(""),
        },
        40 => Entry::Header {
            name: header::IF_MODIFIED_SINCE,
            value: HeaderValue::from_static(""),
        },
        41 => Entry::Header {
            name: header::IF_NONE_MATCH,
            value: HeaderValue::from_static(""),
        },
        42 => Entry::Header {
            name: header::IF_RANGE,
            value: HeaderValue::from_static(""),
        },
        43 => Entry::Header {
            name: header::IF_UNMODIFIED_SINCE,
            value: HeaderValue::from_static(""),
        },
        44 => Entry::Header {
            name: header::LAST_MODIFIED,
            value: HeaderValue::from_static(""),
        },
        45 => Entry::Header {
            name: header::LINK,
            value: HeaderValue::from_static(""),
        },
        46 => Entry::Header {
            name: header::LOCATION,
            value: HeaderValue::from_static(""),
        },
        47 => Entry::Header {
            name: header::MAX_FORWARDS,
            value: HeaderValue::from_static(""),
        },
        48 => Entry::Header {
            name: header::PROXY_AUTHENTICATE,
            value: HeaderValue::from_static(""),
        },
        49 => Entry::Header {
            name: header::PROXY_AUTHORIZATION,
            value: HeaderValue::from_static(""),
        },
        50 => Entry::Header {
            name: header::RANGE,
            value: HeaderValue::from_static(""),
        },
        51 => Entry::Header {
            name: header::REFERER,
            value: HeaderValue::from_static(""),
        },
        52 => Entry::Header {
            name: header::REFRESH,
            value: HeaderValue::from_static(""),
        },
        53 => Entry::Header {
            name: header::RETRY_AFTER,
            value: HeaderValue::from_static(""),
        },
        54 => Entry::Header {
            name: header::SERVER,
            value: HeaderValue::from_static(""),
        },
        55 => Entry::Header {
            name: header::SET_COOKIE,
            value: HeaderValue::from_static(""),
        },
        56 => Entry::Header {
            name: header::STRICT_TRANSPORT_SECURITY,
            value: HeaderValue::from_static(""),
        },
        57 => Entry::Header {
            name: header::TRANSFER_ENCODING,
            value: HeaderValue::from_static(""),
        },
        58 => Entry::Header {
            name: header::USER_AGENT,
            value: HeaderValue::from_static(""),
        },
        59 => Entry::Header {
            name: header::VARY,
            value: HeaderValue::from_static(""),
        },
        60 => Entry::Header {
            name: header::VIA,
            value: HeaderValue::from_static(""),
        },
        61 => Entry::Header {
            name: header::WWW_AUTHENTICATE,
            value: HeaderValue::from_static(""),
        },
        _ => unreachable!(),
    }
}
