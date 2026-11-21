package screencode

import "testing"

func sampleHeader() Header {
	return NewHeader(HeaderSpec{
		ModulePx:    2,
		GridW:       1244,
		GridH:       656,
		BlockBytes:  4096,
		K:           100,
		M:           10,
		TotalShards: 8470,
		FileLen:     31457280,
		HashPrefix:  [8]byte{1, 2, 3, 4, 5, 6, 7, 8},
	})
}

func TestHeaderRoundTrip(t *testing.T) {
	header := sampleHeader()
	bytes := header.ToBytes()
	got, ok := ParseHeader(&bytes)
	if !ok {
		t.Fatal("解析失败")
	}
	if got != header {
		t.Fatalf("往返不一致: %+v != %+v", got, header)
	}
	// 尾部 38..40 必须是保留的 0
	if bytes[38] != 0 || bytes[39] != 0 {
		t.Fatalf("保留字节被写脏: %v", bytes[38:40])
	}
	if bytes[0] != 'S' || bytes[1] != 'C' {
		t.Fatalf("magic 错误: %v", bytes[:2])
	}
}

func TestHeaderRejectsCorruption(t *testing.T) {
	header := sampleHeader()

	broken := header.ToBytes()
	broken[10] ^= 0xFF // 破坏 k
	if _, ok := ParseHeader(&broken); ok {
		t.Fatal("k 被破坏仍然解析成功")
	}

	broken = header.ToBytes()
	broken[0] = 'X'
	if _, ok := ParseHeader(&broken); ok {
		t.Fatal("magic 错误仍然解析成功")
	}

	broken = header.ToBytes()
	broken[2] = FormatVersion + 1
	// 版本号变了但 CRC 未更新，先过 CRC 就会失败；这里只要求解析失败
	if _, ok := ParseHeader(&broken); ok {
		t.Fatal("版本错误仍然解析成功")
	}
}

func TestHeaderBitBytesHasThreeCopies(t *testing.T) {
	header := sampleHeader()
	bits := header.ToBitBytes()
	if len(bits) != int(HeaderW*HeaderH)/8 {
		t.Fatalf("帧头位流 %d 字节，期望 %d", len(bits), int(HeaderW*HeaderH)/8)
	}
	if got, ok := ParseBitBytes(bits); !ok || got != header {
		t.Fatal("整份位流解析失败")
	}

	broken := append([]byte(nil), bits...)
	for i := 1; i < HeaderCopies; i++ {
		broken[i*HeaderBytes] ^= 0xFF
	}
	if got, ok := ParseBitBytes(broken); !ok || got != header {
		t.Fatal("后两份坏掉后未能从第一份恢复")
	}

	// 三份全坏必须失败
	allBroken := append([]byte(nil), bits...)
	for i := 0; i < HeaderCopies; i++ {
		allBroken[i*HeaderBytes+3] ^= 0xFF
	}
	if _, ok := ParseBitBytes(allBroken); ok {
		t.Fatal("三份都坏仍然解析成功")
	}
}

func TestHeaderGroups(t *testing.T) {
	header := sampleHeader()
	if header.Groups() != 77 {
		t.Fatalf("分组 = %d，期望 77", header.Groups())
	}
	if header.DataBlocks() != 7700 {
		t.Fatalf("数据分片 = %d，期望 7700", header.DataBlocks())
	}
	if header.SlotBytes() != 4104 {
		t.Fatalf("slot 字节 = %d，期望 4104", header.SlotBytes())
	}
}
