package screencode

import "testing"

func TestPlanFECReference(t *testing.T) {
	plan, err := PlanFEC(31_457_280, 4096, 0.10)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	want := FECPlan{K: 100, M: 10, DataBlocks: 7680, Groups: 77, TotalShards: 8470}
	if plan != want {
		t.Fatalf("规划 = %+v，期望 %+v", plan, want)
	}
}

func TestPlanFECEdgeCases(t *testing.T) {
	// 单块文件：k=1，parity>0 时至少 1 个校验分片
	plan, err := PlanFEC(1000, 4096, 0.10)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	if plan.K != 1 || plan.M != 1 || plan.Groups != 1 || plan.TotalShards != 2 {
		t.Fatalf("单块规划 = %+v", plan)
	}

	// parity = 0 关闭纠删
	plan, err = PlanFEC(1000, 4096, 0)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	if plan.M != 0 || plan.TotalShards != 1 {
		t.Fatalf("关闭纠删后规划 = %+v", plan)
	}

	// 恰好整除
	plan, err = PlanFEC(8192, 4096, 0.5)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	if plan.DataBlocks != 2 || plan.K != 2 || plan.M != 1 || plan.TotalShards != 3 {
		t.Fatalf("整除规划 = %+v", plan)
	}

	for _, tc := range []struct {
		name       string
		fileLen    int
		blockBytes int
		parity     float64
	}{
		{"empty", 0, 4096, 0.1},
		{"block0", 1, 0, 0.1},
		{"parityTooBig", 1, 4096, 1.5},
		{"parityNegative", 1, 4096, -0.1},
	} {
		t.Run(tc.name, func(t *testing.T) {
			if _, err := PlanFEC(tc.fileLen, tc.blockBytes, tc.parity); err == nil {
				t.Fatal("期望报错，实际通过")
			}
		})
	}
}

func TestFECGlobalIndex(t *testing.T) {
	plan := FECPlan{K: 3, M: 2, DataBlocks: 7, Groups: 3, TotalShards: 15}
	want := []int{0, 1, 2, 5, 6, 7, 10}
	for block, expect := range want {
		if got := plan.GlobalIndex(block); got != expect {
			t.Fatalf("第 %d 块全局下标 = %d，期望 %d", block, got, expect)
		}
	}
	for block, expect := range want {
		group, index := plan.Locate(expect)
		if group != block/3 || index != block%3 {
			t.Fatalf("Locate(%d) = (%d,%d)", expect, group, index)
		}
	}
}

// 用可以手算的小例子锁死「Vandermonde 转系统码」的系数：
// k=2, m=1 的校验行是 [3, 2]（GF(256)），所以 parity = 3*d0 ^ 2*d1。
func TestEncodeShardsSmallGroup(t *testing.T) {
	file := []byte{1, 2}
	plan, err := PlanFEC(len(file), 1, 0.5)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	if plan.K != 2 || plan.M != 1 {
		t.Fatalf("规划 = %+v，期望 k=2 m=1", plan)
	}
	shards, err := EncodeShards(file, plan, 1)
	if err != nil {
		t.Fatalf("编码失败: %v", err)
	}
	if len(shards) != 3 {
		t.Fatalf("分片数 = %d，期望 3", len(shards))
	}
	if shards[0][0] != 1 || shards[1][0] != 2 {
		t.Fatalf("数据分片被改动: %v %v", shards[0], shards[1])
	}
	if shards[2][0] != 7 {
		t.Fatalf("校验分片 = %d，期望 7", shards[2][0])
	}
}

func TestEncodeShardsSingleDataShard(t *testing.T) {
	// k=1 时校验分片就是数据分片的副本（系统码第 1 行系数为 1）
	file := []byte{0xA5}
	plan, err := PlanFEC(len(file), 1, 1.0)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	if plan.K != 1 || plan.M != 1 {
		t.Fatalf("规划 = %+v", plan)
	}
	shards, err := EncodeShards(file, plan, 1)
	if err != nil {
		t.Fatalf("编码失败: %v", err)
	}
	if shards[1][0] != 0xA5 {
		t.Fatalf("校验分片 = %#x，期望 0xa5", shards[1][0])
	}
}

func TestEncodeShardsWithoutParity(t *testing.T) {
	file := make([]byte, 5)
	for i := range file {
		file[i] = byte(i + 1)
	}
	plan, err := PlanFEC(len(file), 4, 0)
	if err != nil {
		t.Fatalf("规划失败: %v", err)
	}
	shards, err := EncodeShards(file, plan, 4)
	if err != nil {
		t.Fatalf("编码失败: %v", err)
	}
	if len(shards) != 2 {
		t.Fatalf("分片数 = %d，期望 2", len(shards))
	}
	if shards[0][3] != 4 {
		t.Fatalf("第 1 片 = %v", shards[0])
	}
	// 第 2 片只有第 1 个字节有效，其余补 0
	if shards[1][0] != 5 || shards[1][1] != 0 {
		t.Fatalf("第 2 片 = %v", shards[1])
	}
}
