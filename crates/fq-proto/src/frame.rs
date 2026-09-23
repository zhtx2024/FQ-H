//! TCP 分帧:4 字节大端长度前缀 + 载荷。
//!
//! 本模块刻意**不依赖任何 IO 库**:它只提供纯函数与状态机,这样既能被
//! 单元测试彻底覆盖(半包/粘包/超长帧),也能被 tokio / 同步代码复用。
//!
//! 超长帧必须在**读到长度前缀的第一时间**拒绝,而不是先缓冲再判断 ——
//! 否则一个恶意声明的 4 GiB 长度就能让进程 OOM。

use crate::error::{Error, Result};

/// 默认最大帧长度(1 MiB)。文件分块默认 256 KiB,留足余量。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 长度前缀字节数。
pub const LENGTH_PREFIX_BYTES: usize = 4;

/// 内部缓冲区触发压缩整理的阈值。
const COMPACT_THRESHOLD: usize = 64 * 1024;

/// 把载荷封装为一帧(4 字节大端长度 + 载荷)。
pub fn encode_frame(payload: &[u8], max_frame: usize) -> Result<Vec<u8>> {
    let too_large = || Error::FrameTooLarge {
        size: payload.len(),
        max: max_frame,
    };
    if payload.len() > max_frame {
        return Err(too_large());
    }
    let len = u32::try_from(payload.len()).map_err(|_| too_large())?;

    let mut out = Vec::with_capacity(LENGTH_PREFIX_BYTES + payload.len());
    out.extend_from_slice(&len.to_be_bytes());
    out.extend_from_slice(payload);
    Ok(out)
}

/// 重新组装长度前缀帧的解码器。
///
/// 用法:反复 `feed()` 收到的字节,然后循环 `next_frame()` 直到返回 `None`。
#[derive(Debug)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    start: usize,
    max_frame: usize,
}

impl FrameDecoder {
    /// 以自定义上限创建解码器。
    pub fn new(max_frame: usize) -> Self {
        Self {
            buf: Vec::new(),
            start: 0,
            max_frame,
        }
    }

    /// 以 [`MAX_FRAME_BYTES`] 为上限创建解码器。
    pub fn with_default_limit() -> Self {
        Self::new(MAX_FRAME_BYTES)
    }

    /// 追加收到的字节,并在能判定长度非法时立即报错。
    pub fn feed(&mut self, data: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(data);
        self.reject_oversized_if_known()
    }

    /// 取出下一个完整帧的载荷;数据不足时返回 `Ok(None)`。
    pub fn next_frame(&mut self) -> Result<Option<Vec<u8>>> {
        if self.available() < LENGTH_PREFIX_BYTES {
            self.compact();
            return Ok(None);
        }

        let len = self.peek_len()?;
        if len > self.max_frame {
            return Err(Error::FrameTooLarge {
                size: len,
                max: self.max_frame,
            });
        }

        let total = LENGTH_PREFIX_BYTES + len;
        if self.available() < total {
            return Ok(None);
        }

        let payload = self.buf[self.start + LENGTH_PREFIX_BYTES..self.start + total].to_vec();
        self.start += total;
        if self.available() == 0 {
            self.buf.clear();
            self.start = 0;
        } else if self.start >= COMPACT_THRESHOLD {
            self.compact();
        }
        Ok(Some(payload))
    }

    /// 当前仍在缓冲区中、尚未组成完整帧的字节数。
    pub fn buffered_bytes(&self) -> usize {
        self.available()
    }

    /// 缓冲区是否已排空。
    pub fn is_idle(&self) -> bool {
        self.available() == 0
    }

    fn available(&self) -> usize {
        self.buf.len() - self.start
    }

    fn reject_oversized_if_known(&self) -> Result<()> {
        if self.available() < LENGTH_PREFIX_BYTES {
            return Ok(());
        }
        let len = self.peek_len()?;
        if len > self.max_frame {
            return Err(Error::FrameTooLarge {
                size: len,
                max: self.max_frame,
            });
        }
        Ok(())
    }

    fn peek_len(&self) -> Result<usize> {
        let head = self
            .buf
            .get(self.start..self.start + LENGTH_PREFIX_BYTES)
            .ok_or(Error::FrameHeaderTruncated(self.available()))?;
        let arr: [u8; LENGTH_PREFIX_BYTES] = head
            .try_into()
            .map_err(|_| Error::FrameHeaderTruncated(self.available()))?;
        Ok(u32::from_be_bytes(arr) as usize)
    }

    fn compact(&mut self) {
        if self.start > 0 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn frame_roundtrip() {
        let payload = b"hello \xe4\xb8\x96\xe7\x95\x8c";
        let framed = encode_frame(payload, MAX_FRAME_BYTES).unwrap();
        assert_eq!(&framed[..4], &(payload.len() as u32).to_be_bytes());

        let mut dec = FrameDecoder::with_default_limit();
        dec.feed(&framed).unwrap();
        assert_eq!(dec.next_frame().unwrap().unwrap(), payload);
        assert!(dec.next_frame().unwrap().is_none());
        assert!(dec.is_idle());
    }

    #[test]
    fn rejects_oversized_payload_at_encode() {
        let err = encode_frame(&[0u8; 32], 16).unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { size: 32, max: 16 }));
    }

    #[test]
    fn rejects_oversized_declared_length_before_buffering_body() {
        let mut dec = FrameDecoder::new(1024);
        // 声称 4 GiB,但只发 4 字节头
        let err = dec.feed(&u32::MAX.to_be_bytes()).unwrap_err();
        assert!(matches!(err, Error::FrameTooLarge { .. }));
        assert!(
            dec.buffered_bytes() <= LENGTH_PREFIX_BYTES,
            "必须在读到长度前缀时立即拒绝,不得继续缓冲"
        );
    }

    #[test]
    fn reassembles_across_multiple_feeds() {
        let payload = vec![7u8; 1000];
        let framed = encode_frame(&payload, MAX_FRAME_BYTES).unwrap();
        let mut dec = FrameDecoder::with_default_limit();

        for chunk in framed.chunks(7) {
            dec.feed(chunk).unwrap();
        }
        assert_eq!(dec.next_frame().unwrap().unwrap(), payload);
    }

    #[test]
    fn splits_sticky_frames() {
        let a = b"first".to_vec();
        let b = b"second-longer".to_vec();
        let mut wire = encode_frame(&a, MAX_FRAME_BYTES).unwrap();
        wire.extend_from_slice(&encode_frame(&b, MAX_FRAME_BYTES).unwrap());

        let mut dec = FrameDecoder::with_default_limit();
        dec.feed(&wire).unwrap();
        assert_eq!(dec.next_frame().unwrap().unwrap(), a);
        assert_eq!(dec.next_frame().unwrap().unwrap(), b);
        assert!(dec.next_frame().unwrap().is_none());
        assert!(dec.is_idle());
    }

    #[test]
    fn byte_by_byte_feed_is_stable() {
        let a = vec![1u8; 3];
        let b = vec![2u8; 260];
        let mut wire = encode_frame(&a, MAX_FRAME_BYTES).unwrap();
        wire.extend_from_slice(&encode_frame(&b, MAX_FRAME_BYTES).unwrap());

        let mut dec = FrameDecoder::with_default_limit();
        let mut got = Vec::new();
        for byte in &wire {
            dec.feed(std::slice::from_ref(byte)).unwrap();
            while let Some(frame) = dec.next_frame().unwrap() {
                got.push(frame);
            }
        }
        assert_eq!(got, vec![a, b]);
    }

    #[test]
    fn zero_length_frame_is_legal() {
        let framed = encode_frame(&[], MAX_FRAME_BYTES).unwrap();
        let mut dec = FrameDecoder::with_default_limit();
        dec.feed(&framed).unwrap();
        assert_eq!(dec.next_frame().unwrap().unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn exact_limit_is_accepted() {
        let payload = vec![9u8; 64];
        let framed = encode_frame(&payload, 64).unwrap();
        let mut dec = FrameDecoder::new(64);
        dec.feed(&framed).unwrap();
        assert_eq!(dec.next_frame().unwrap().unwrap(), payload);
    }

    #[test]
    fn many_frames_do_not_grow_buffer_unbounded() {
        let payload = vec![3u8; 512];
        let framed = encode_frame(&payload, MAX_FRAME_BYTES).unwrap();
        let mut dec = FrameDecoder::with_default_limit();
        for _ in 0..256 {
            dec.feed(&framed).unwrap();
            assert!(dec.next_frame().unwrap().is_some());
        }
        assert!(dec.is_idle());
        assert_eq!(dec.buffered_bytes(), 0);
    }
}
