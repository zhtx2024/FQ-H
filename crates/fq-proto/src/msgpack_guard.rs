//! MessagePack 结构预校验(安全关键)。
//!
//! 背景:MessagePack 的长度字段是**攻击者可控**的。`rmp-serde` 在读到
//! `str32/bin32/array32/map32` 的长度后,会按该长度预分配内存。一个 20 字节
//! 的恶意报文就能声明 4 GiB 长度,Rust 的分配失败会 **abort 整个进程**
//! (无法用 `catch_unwind` 兜住)—— 这是局域网协议必须堵死的 DoS 面。
//!
//! 对策:在交给 serde 之前,先用本模块做一次**结构与长度校验**遍历:
//!
//! 1. 任何容器的声明长度不得超过"帧内剩余字节数"(元素至少占 1 字节)。
//! 2. 嵌套深度不得超过 [`MAX_DEPTH`],防止栈溢出。
//! 3. 整体必须恰好消费完,不允许尾部残留(避免走私/歧义字节)。
//!
//! 校验通过后,serde 面对的输入规模已经被帧上限约束,不再可能触发超大分配。

use crate::error::{Error, Result};

/// 允许的最大嵌套深度。serde 自身默认 128,这里收紧到 64。
pub const MAX_DEPTH: usize = 64;

/// 校验 `bytes` 是否为**恰好一个**结构完整的 MessagePack 值。
pub fn validate(bytes: &[u8]) -> Result<()> {
    let mut reader = Reader { buf: bytes, pos: 0 };
    validate_value(&mut reader, 0)?;
    if reader.remaining() != 0 {
        return Err(Error::MalformedMsgpack(format!(
            "尾部存在 {} 字节多余数据",
            reader.remaining()
        )));
    }
    Ok(())
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or_else(|| Error::MalformedMsgpack("长度计算溢出".into()))?;
        if end > self.buf.len() {
            return Err(Error::MalformedMsgpack(format!(
                "声明长度 {n} 超出帧边界(剩余 {})",
                self.remaining()
            )));
        }
        let slice = &self.buf[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8> {
        let byte = *self
            .buf
            .get(self.pos)
            .ok_or_else(|| Error::MalformedMsgpack("数据在读取类型标记时结束".into()))?;
        self.pos += 1;
        Ok(byte)
    }

    fn be_u16(&mut self) -> Result<usize> {
        let s = self.take(2)?;
        Ok(u16::from_be_bytes([s[0], s[1]]) as usize)
    }

    fn be_u32(&mut self) -> Result<usize> {
        let s = self.take(4)?;
        Ok(u32::from_be_bytes([s[0], s[1], s[2], s[3]]) as usize)
    }

    /// 容器元素个数合理性检查:每个元素至少占 1 字节。
    fn check_container_len(&self, len: usize) -> Result<()> {
        if len > self.remaining() {
            return Err(Error::MalformedMsgpack(format!(
                "容器声明 {len} 个元素,但仅剩 {} 字节",
                self.remaining()
            )));
        }
        Ok(())
    }
}

fn validate_value(reader: &mut Reader<'_>, depth: usize) -> Result<()> {
    if depth > MAX_DEPTH {
        return Err(Error::MalformedMsgpack(format!(
            "嵌套深度超过上限 {MAX_DEPTH}"
        )));
    }

    let marker = reader.u8()?;
    match marker {
        // positive fixint / nil / false / true / negative fixint
        0x00..=0x7f | 0xc0 | 0xc2 | 0xc3 | 0xe0..=0xff => {}

        // fixmap:低 4 位是键值对数量
        0x80..=0x8f => {
            let pairs = (marker & 0x0f) as usize;
            validate_map(reader, pairs, depth)?;
        }
        // fixarray:低 4 位是元素数量
        0x90..=0x9f => {
            let len = (marker & 0x0f) as usize;
            validate_array(reader, len, depth)?;
        }
        // fixstr:低 5 位是字节长度(UTF-8 长度后续由 serde 校验)
        0xa0..=0xbf => {
            let len = (marker & 0x1f) as usize;
            reader.take(len)?;
        }

        // 0xc1 在 MessagePack 规范中保留未用,出现即为非法
        0xc1 => return Err(Error::MalformedMsgpack("非法的类型标记 0xc1".into())),

        // bin8 / bin16 / bin32
        0xc4 => {
            let len = reader.u8()? as usize;
            reader.take(len)?;
        }
        0xc5 => {
            let len = reader.be_u16()?;
            reader.take(len)?;
        }
        0xc6 => {
            let len = reader.be_u32()?;
            reader.take(len)?;
        }

        // ext8 / ext16 / ext32:(长度,类型字节,数据)
        0xc7 => {
            let len = reader.u8()? as usize;
            reader.take(1)?;
            reader.take(len)?;
        }
        0xc8 => {
            let len = reader.be_u16()?;
            reader.take(1)?;
            reader.take(len)?;
        }
        0xc9 => {
            let len = reader.be_u32()?;
            reader.take(1)?;
            reader.take(len)?;
        }

        // float32 / float64
        0xca => {
            reader.take(4)?;
        }
        0xcb => {
            reader.take(8)?;
        }

        // uint8/16/32/64
        0xcc => {
            reader.take(1)?;
        }
        0xcd => {
            reader.take(2)?;
        }
        0xce => {
            reader.take(4)?;
        }
        0xcf => {
            reader.take(8)?;
        }

        // int8/16/32/64
        0xd0 => {
            reader.take(1)?;
        }
        0xd1 => {
            reader.take(2)?;
        }
        0xd2 => {
            reader.take(4)?;
        }
        0xd3 => {
            reader.take(8)?;
        }

        // fixext1/2/4/8/16:(类型字节,固定长度数据)
        0xd4 => {
            reader.take(1)?;
            reader.take(1)?;
        }
        0xd5 => {
            reader.take(1)?;
            reader.take(2)?;
        }
        0xd6 => {
            reader.take(1)?;
            reader.take(4)?;
        }
        0xd7 => {
            reader.take(1)?;
            reader.take(8)?;
        }
        0xd8 => {
            reader.take(1)?;
            reader.take(16)?;
        }

        // str8 / str16 / str32
        0xd9 => {
            let len = reader.u8()? as usize;
            reader.take(len)?;
        }
        0xda => {
            let len = reader.be_u16()?;
            reader.take(len)?;
        }
        0xdb => {
            let len = reader.be_u32()?;
            reader.take(len)?;
        }

        // array16 / array32
        0xdc => {
            let len = reader.be_u16()?;
            validate_array(reader, len, depth)?;
        }
        0xdd => {
            let len = reader.be_u32()?;
            validate_array(reader, len, depth)?;
        }

        // map16 / map32
        0xde => {
            let pairs = reader.be_u16()?;
            validate_map(reader, pairs, depth)?;
        }
        0xdf => {
            let pairs = reader.be_u32()?;
            validate_map(reader, pairs, depth)?;
        }
    }

    Ok(())
}

fn validate_array(reader: &mut Reader<'_>, len: usize, depth: usize) -> Result<()> {
    reader.check_container_len(len)?;
    for _ in 0..len {
        validate_value(reader, depth + 1)?;
    }
    Ok(())
}

fn validate_map(reader: &mut Reader<'_>, pairs: usize, depth: usize) -> Result<()> {
    // 每对键值至少 2 字节
    if pairs > reader.remaining() / 2 {
        return Err(Error::MalformedMsgpack(format!(
            "映射声明 {pairs} 个键值对,但仅剩 {} 字节",
            reader.remaining()
        )));
    }
    for _ in 0..pairs {
        validate_value(reader, depth + 1)?;
        validate_value(reader, depth + 1)?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn encoded(value: &impl serde::Serialize) -> Vec<u8> {
        rmp_serde::to_vec_named(value).unwrap()
    }

    #[test]
    fn accepts_simple_scalars() {
        for value in [1u8, 255, 0] {
            assert!(validate(&encoded(&value)).is_ok());
        }
        assert!(validate(&encoded(&"hi")).is_ok());
        assert!(validate(&encoded(&vec![1u8, 2, 3])).is_ok());
    }

    #[test]
    fn rejects_trailing_garbage() {
        let mut bytes = encoded(&"hello");
        bytes.push(0xc0);
        let err = validate(&bytes).unwrap_err();
        assert!(matches!(err, Error::MalformedMsgpack(_)));
    }

    #[test]
    fn rejects_reserved_marker() {
        // 0xc1 在 MessagePack 规范中保留未用
        let err = validate(&[0xc1]).unwrap_err();
        assert!(matches!(err, Error::MalformedMsgpack(_)));
    }

    #[test]
    fn rejects_huge_declared_str_length() {
        // str32 声明 0xFFFF_FFFF 字节,实际没有数据 —— 必须在分配前拒绝
        let bytes = [0xdb, 0xff, 0xff, 0xff, 0xff];
        let err = validate(&bytes).unwrap_err();
        assert!(matches!(err, Error::MalformedMsgpack(_)));
    }

    #[test]
    fn rejects_huge_declared_array_length() {
        // array32 声明 0xFFFF_FFFF 个元素
        let bytes = [0xdd, 0xff, 0xff, 0xff, 0xff];
        let err = validate(&bytes).unwrap_err();
        assert!(matches!(err, Error::MalformedMsgpack(_)));
    }

    #[test]
    fn rejects_huge_declared_map_length() {
        let bytes = [0xdf, 0xff, 0xff, 0xff, 0xff];
        let err = validate(&bytes).unwrap_err();
        assert!(matches!(err, Error::MalformedMsgpack(_)));
    }

    #[test]
    fn rejects_deep_nesting_instead_of_overflowing_stack() {
        // 用 fixarray(1 个元素)嵌套上千层
        let bytes = vec![0x91; MAX_DEPTH * 4];
        let err = validate(&bytes).unwrap_err();
        assert!(matches!(err, Error::MalformedMsgpack(_)));
    }

    #[test]
    fn rejects_truncated_input() {
        let full = encoded(&vec![1u8, 2, 3, 4, 5]);
        for cut in 1..full.len() {
            assert!(validate(&full[..cut]).is_err(), "截断到 {cut} 字节时应报错");
        }
    }

    #[test]
    fn accepts_nested_structures() {
        let value =
            std::collections::HashMap::from([("key".to_string(), vec![Some(1u32), None, Some(3)])]);
        assert!(validate(&encoded(&value)).is_ok());
    }
}
