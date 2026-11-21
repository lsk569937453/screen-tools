package screencode

import (
	"encoding/binary"
	"hash/crc32"
)

// Header 是 40 字节帧头；同一帧里重复 3 份，任意一份 CRC 通过即可用。
type Header struct {
	Version     uint8
	ModulePx    uint8
	GridW       uint16
	GridH       uint16
	BlockBytes  uint16
	K           uint16
	M           uint16
	TotalShards uint32
	FileLen     uint64
	HashPrefix  [8]byte
}

// HeaderSpec 是构造帧头所需的全部字段，避免构造函数参数过多。
type HeaderSpec struct {
	ModulePx    uint32
	GridW       uint32
	GridH       uint32
	BlockBytes  int
	K           int
	M           int
	TotalShards int
	FileLen     uint64
	HashPrefix  [8]byte
}

// NewHeader 按码制字段宽度裁剪后构造帧头，与 Rust `Header::new` 的取值一致。
func NewHeader(spec HeaderSpec) Header {
	modulePx := spec.ModulePx
	if modulePx > 255 {
		modulePx = 255
	}
	return Header{
		Version:     FormatVersion,
		ModulePx:    uint8(modulePx),
		GridW:       uint16(spec.GridW),
		GridH:       uint16(spec.GridH),
		BlockBytes:  uint16(spec.BlockBytes),
		K:           uint16(spec.K),
		M:           uint16(spec.M),
		TotalShards: uint32(spec.TotalShards),
		FileLen:     spec.FileLen,
		HashPrefix:  spec.HashPrefix,
	}
}

// SlotBytes 是每个 slot 的字节数。
func (h Header) SlotBytes() int { return int(h.BlockBytes) + SlotOverhead }

// Groups 是纠删码分组数；分片数与 (k+m) 不整除时返回 0。
func (h Header) Groups() int {
	per := int(h.K) + int(h.M)
	if per == 0 {
		return 0
	}
	return int(h.TotalShards) / per
}

// DataBlocks 是数据分片总数。
func (h Header) DataBlocks() int { return h.Groups() * int(h.K) }

// ToBytes 序列化成 40 字节（含 CRC32，小端）。
func (h Header) ToBytes() [HeaderBytes]byte {
	var buf [HeaderBytes]byte
	buf[0] = 'S'
	buf[1] = 'C'
	buf[2] = h.Version
	buf[3] = h.ModulePx
	binary.LittleEndian.PutUint16(buf[4:6], h.GridW)
	binary.LittleEndian.PutUint16(buf[6:8], h.GridH)
	binary.LittleEndian.PutUint16(buf[8:10], h.BlockBytes)
	binary.LittleEndian.PutUint16(buf[10:12], h.K)
	binary.LittleEndian.PutUint16(buf[12:14], h.M)
	binary.LittleEndian.PutUint32(buf[14:18], h.TotalShards)
	binary.LittleEndian.PutUint64(buf[18:26], h.FileLen)
	copy(buf[26:34], h.HashPrefix[:])
	// CRC 只覆盖 0..34，38..40 保留为 0。
	binary.LittleEndian.PutUint32(buf[34:38], crc32.ChecksumIEEE(buf[:34]))
	return buf
}

// ParseHeader 解析并校验 40 字节帧头；magic / CRC / 版本 / 字段合法性任一不符返回 false。
func ParseHeader(buf *[HeaderBytes]byte) (Header, bool) {
	if buf[0] != 'S' || buf[1] != 'C' {
		return Header{}, false
	}
	want := binary.LittleEndian.Uint32(buf[34:38])
	if want != crc32.ChecksumIEEE(buf[:34]) {
		return Header{}, false
	}
	if buf[2] != FormatVersion {
		return Header{}, false
	}

	header := Header{
		Version:     buf[2],
		ModulePx:    buf[3],
		GridW:       binary.LittleEndian.Uint16(buf[4:6]),
		GridH:       binary.LittleEndian.Uint16(buf[6:8]),
		BlockBytes:  binary.LittleEndian.Uint16(buf[8:10]),
		K:           binary.LittleEndian.Uint16(buf[10:12]),
		M:           binary.LittleEndian.Uint16(buf[12:14]),
		TotalShards: binary.LittleEndian.Uint32(buf[14:18]),
		FileLen:     binary.LittleEndian.Uint64(buf[18:26]),
	}
	copy(header.HashPrefix[:], buf[26:34])

	per := uint32(header.K) + uint32(header.M)
	if uint32(header.GridW) < HeaderW+BorderModules*2 ||
		uint32(header.GridH) < HeaderH+BorderModules*2 ||
		header.BlockBytes == 0 ||
		header.K == 0 ||
		per > 256 ||
		header.TotalShards == 0 ||
		header.TotalShards%per != 0 ||
		header.FileLen == 0 {
		return Header{}, false
	}
	return header, true
}

// ToBitBytes 生成帧头位流：3 份副本 + 0 填充，正好 64×16 / 8 = 128 字节。
func (h Header) ToBitBytes() []byte {
	want := int(HeaderW*HeaderH) / 8
	out := make([]byte, 0, want)
	single := h.ToBytes()
	for i := 0; i < HeaderCopies; i++ {
		out = append(out, single[:]...)
	}
	for len(out) < want {
		out = append(out, 0)
	}
	return out
}

// ParseBitBytes 从帧头位流里挑出第一份 CRC 通过的帧头。
func ParseBitBytes(data []byte) (Header, bool) {
	for i := 0; i < HeaderCopies; i++ {
		start := i * HeaderBytes
		end := start + HeaderBytes
		if end > len(data) {
			break
		}
		var buf [HeaderBytes]byte
		copy(buf[:], data[start:end])
		if header, ok := ParseHeader(&buf); ok {
			return header, true
		}
	}
	return Header{}, false
}
