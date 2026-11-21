package screencode

import (
	"errors"
	"fmt"
	"math"
)

// FECPlan 是文件级纠删码分组：k 个数据分片 + m 个校验分片为一组。
type FECPlan struct {
	K           int
	M           int
	DataBlocks  int
	Groups      int
	TotalShards int
}

// ShardBytes 是分片在 slot 里的字节数。
func (p FECPlan) ShardBytes(blockBytes int) int { return blockBytes + SlotOverhead }

// GlobalIndex 是第 block 个数据分片的全局下标。
func (p FECPlan) GlobalIndex(block int) int {
	per := p.K + p.M
	return (block/p.K)*per + block%p.K
}

// Locate 把全局下标还原成 (组, 组内序号)。
func (p FECPlan) Locate(global int) (int, int) {
	per := p.K + p.M
	return global / per, global % per
}

// PlanFEC 规划纠删码分组；取整与夹取规则与 Rust `fec::plan` 完全一致。
func PlanFEC(fileLen, blockBytes int, parity float64) (FECPlan, error) {
	if fileLen == 0 {
		return FECPlan{}, errors.New("文件为空，无法生成")
	}
	if blockBytes <= 0 {
		return FECPlan{}, errors.New("block_bytes 必须 >= 1")
	}
	if parity < 0 || parity > 1 || math.IsNaN(parity) {
		return FECPlan{}, errors.New("parity 必须在 0.0..=1.0 之间")
	}

	dataBlocks := (fileLen + blockBytes - 1) / blockBytes
	k := dataBlocks
	if k < 1 {
		k = 1
	}
	if k > FecKMax {
		k = FecKMax
	}

	m := 0
	if parity > 0 {
		m = int(math.Round(float64(k) * parity))
		if m < 1 {
			m = 1
		}
	}
	if m > FecMaxShardsPerGroup-k {
		m = FecMaxShardsPerGroup - k
	}

	groups := (dataBlocks + k - 1) / k
	return FECPlan{
		K:           k,
		M:           m,
		DataBlocks:  dataBlocks,
		Groups:      groups,
		TotalShards: groups * (k + m),
	}, nil
}

// EncodeShards 把文件编码成全部数据分片 + 校验分片（每个分片 blockBytes 字节）。
func EncodeShards(file []byte, plan FECPlan, blockBytes int) ([][]byte, error) {
	if blockBytes <= 0 {
		return nil, errors.New("block_bytes 必须 >= 1")
	}
	if plan.TotalShards <= 0 || plan.K <= 0 {
		return nil, errors.New("纠删码分组无效")
	}

	shards := make([][]byte, plan.TotalShards)
	for i := range shards {
		shards[i] = make([]byte, blockBytes)
	}

	for block := 0; block < plan.DataBlocks; block++ {
		start := block * blockBytes
		if start >= len(file) {
			break
		}
		end := start + blockBytes
		if end > len(file) {
			end = len(file)
		}
		idx := plan.GlobalIndex(block)
		if idx < 0 || idx >= len(shards) {
			return nil, fmt.Errorf("数据分片 %d 的全局下标 %d 越界", block, idx)
		}
		copy(shards[idx], file[start:end])
	}

	if plan.M == 0 {
		return shards, nil
	}

	codec, err := newRSCodec(plan.K, plan.M)
	if err != nil {
		return nil, err
	}
	per := plan.K + plan.M
	for group := 0; group < plan.Groups; group++ {
		base := group * per
		if base+per > len(shards) {
			return nil, fmt.Errorf("第 %d 组超出分片总数 %d", group, len(shards))
		}
		if err := codec.encode(shards[base : base+per]); err != nil {
			return nil, err
		}
	}
	return shards, nil
}
