use std::mem::size_of;

use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

pub fn prefix<T: Serialize>(header: &T, data: Bytes) -> bincode::Result<Bytes> {
    let header = bincode::serialize(header)?;
    let mut buffer = BytesMut::with_capacity(size_of::<u32>() + header.len() + data.len());
    buffer.put_u32(header.len() as u32);
    buffer.put_slice(&header);
    buffer.put(data);
    Ok(buffer.freeze())
}

pub fn drop_prefix<T: for<'de> Deserialize<'de>>(data: Bytes) -> bincode::Result<(T, Bytes)> {
    let mut data = data;
    let header_len = data.get_u32() as usize;
    let header = bincode::deserialize(&data[..header_len])?;
    data.advance(header_len);
    Ok((header, data))
}
