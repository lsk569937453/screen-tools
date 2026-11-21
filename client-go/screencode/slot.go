package screencode

import (
	"encoding/binary"
	"errors"
	"hash/crc32"
)

// EncodeSlot 把第 idx 个分片包成 slot：idx(4B LE) + payload + crc32(idx+payload)(4B LE)。
func EncodeSlot(idx uint32, payload []byte) []byte {
	out := make([]byte, SlotOverhead+len(payload))
	binary.LittleEndian.PutUint32(out[:4], idx)
	copy(out[4:], payload)
	crc := crc32.NewIEEE()
	_, _ = crc.Write(out[:4+len(payload)])
	binary.LittleEndian.PutUint32(out[4+len(payload):], crc.Sum32())
	return out
}

// PadSlot 生成补位 slot：索引为 PadSlotIndex，净荷全 0，接收端按 CRC 通过后跳过。
func PadSlot(blockBytes int) []byte {
	return EncodeSlot(PadSlotIndex, make([]byte, blockBytes))
}

// ParseSlot 校验并拆开一个 slot，返回分片下标与净荷（指向入参的切片，不拷贝）。
func ParseSlot(data []byte, blockBytes int) (uint32, []byte, error) {
	if len(data) != blockBytes+SlotOverhead {
		return 0, nil, errors.New("slot 长度与 block_bytes 不匹配")
	}
	idx := binary.LittleEndian.Uint32(data[:4])
	payload := data[4 : 4+blockBytes]
	want := binary.LittleEndian.Uint32(data[4+blockBytes:])
	if crc32.ChecksumIEEE(data[:4+blockBytes]) != want {
		return 0, nil, errors.New("slot CRC 校验失败")
	}
	return idx, payload, nil
}
