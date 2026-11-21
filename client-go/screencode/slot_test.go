package screencode

import (
	"bytes"
	"encoding/binary"
	"hash/crc32"
	"testing"
)

func TestEncodeSlotLayout(t *testing.T) {
	payload := []byte{1, 2, 3, 4}
	slot := EncodeSlot(7, payload)
	if len(slot) != len(payload)+SlotOverhead {
		t.Fatalf("slot 长度 = %d，期望 %d", len(slot), len(payload)+SlotOverhead)
	}
	if got := binary.LittleEndian.Uint32(slot[:4]); got != 7 {
		t.Fatalf("索引 = %d，期望 7", got)
	}
	if !bytes.Equal(slot[4:8], payload) {
		t.Fatalf("净荷 = %v，期望 %v", slot[4:8], payload)
	}
	// 尾部 CRC 覆盖「索引 + 净荷」
	if got, want := binary.LittleEndian.Uint32(slot[8:]), crc32.ChecksumIEEE(slot[:8]); got != want {
		t.Fatalf("CRC = %#x，期望 %#x", got, want)
	}
}

func TestParseSlotRoundTrip(t *testing.T) {
	payload := []byte{0xDE, 0xAD, 0xBE, 0xEF}
	slot := EncodeSlot(0x01020304, payload)
	idx, got, err := ParseSlot(slot, len(payload))
	if err != nil {
		t.Fatalf("解析失败: %v", err)
	}
	if idx != 0x01020304 {
		t.Fatalf("索引 = %#x", idx)
	}
	if !bytes.Equal(got, payload) {
		t.Fatalf("净荷 = %v", got)
	}
}

func TestParseSlotRejects(t *testing.T) {
	payload := []byte{9, 8, 7}
	slot := EncodeSlot(1, payload)

	if _, _, err := ParseSlot(slot, len(payload)+1); err == nil {
		t.Fatal("长度不匹配仍然解析成功")
	}

	broken := append([]byte(nil), slot...)
	broken[len(broken)-1] ^= 0xFF
	if _, _, err := ParseSlot(broken, len(payload)); err == nil {
		t.Fatal("CRC 被破坏仍然解析成功")
	}

	broken = append([]byte(nil), slot...)
	broken[0] ^= 0xFF
	if _, _, err := ParseSlot(broken, len(payload)); err == nil {
		t.Fatal("索引被破坏仍然解析成功")
	}
}

func TestPadSlot(t *testing.T) {
	slot := PadSlot(16)
	if len(slot) != 16+SlotOverhead {
		t.Fatalf("补位 slot 长度 = %d", len(slot))
	}
	idx, payload, err := ParseSlot(slot, 16)
	if err != nil {
		t.Fatalf("补位 slot 自身 CRC 不通过: %v", err)
	}
	if idx != PadSlotIndex {
		t.Fatalf("补位索引 = %#x，期望 %#x", idx, PadSlotIndex)
	}
	for i, b := range payload {
		if b != 0 {
			t.Fatalf("补位净荷第 %d 字节 = %#x", i, b)
		}
	}
}
