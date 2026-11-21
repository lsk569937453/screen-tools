package screencode

import (
	"errors"
	"fmt"
)

// 本文件实现 GF(2^8) 与「Vandermonde 转系统码」的矩阵运算。
//
// 契约：必须与 Rust `reed-solomon-erasure 6.0.0` 的 galois_8 实现逐字节一致，
// 否则接收端在丢数据分片时会恢复失败。对应关系：
//   - 域多项式 0x11D、生成元 2（crate 的 GENERATING_POLYNOMIAL = 29 = 0x1D）；
//   - exp(a, n)：n == 0 时恒为 1（含 0^0 = 1），a == 0 且 n > 0 时为 0；
//   - Vandermonde：v[r][c] = exp(r, c)，行号直接取字节值；
//   - 系统码：M = V · inverse(V[0:k][0:k])，校验行为 M[k..k+m]。
const (
	gfPolynomial = 0x1d
	gfOrder      = 256
)

var (
	gfLog [gfOrder]byte
	gfExp [2 * gfOrder]byte
	gfMul [gfOrder][gfOrder]byte
)

func init() {
	b := 1
	for log := 0; log < gfOrder-1; log++ {
		gfLog[b] = byte(log)
		b <<= 1
		if b >= gfOrder {
			b = (b - gfOrder) ^ gfPolynomial
		}
	}
	// LOG[0] 保持 0，与 crate 生成的表一致。
	for i := 1; i < gfOrder; i++ {
		log := int(gfLog[i])
		gfExp[log] = byte(i)
		gfExp[log+gfOrder-1] = byte(i)
	}
	for a := 0; a < gfOrder; a++ {
		for c := 0; c < gfOrder; c++ {
			gfMul[a][c] = gfMulSlow(byte(a), byte(c))
		}
	}
}

// gfMulSlow 用 log/exp 表算乘法，只在初始化建表时用。
func gfMulSlow(a, b byte) byte {
	if a == 0 || b == 0 {
		return 0
	}
	return gfExp[int(gfLog[a])+int(gfLog[b])]
}

// gfPow 是 crate 的 exp(a, n)：注意 0^0 = 1。
func gfPow(a byte, n int) byte {
	if n == 0 {
		return 1
	}
	if a == 0 {
		return 0
	}
	exp := int(gfLog[a]) * n
	for exp >= gfOrder-1 {
		exp -= gfOrder - 1
	}
	return gfExp[exp]
}

// gfInv 是乘法逆元；入参 0 视为调用方错误。
func gfInv(a byte) (byte, error) {
	if a == 0 {
		return 0, errors.New("GF(256) 中 0 没有乘法逆元")
	}
	return gfExp[gfOrder-1-int(gfLog[a])], nil
}

// matrix 是行优先的 GF(256) 矩阵。
type matrix struct {
	rows int
	cols int
	data [][]byte
}

func newMatrix(rows, cols int) matrix {
	data := make([][]byte, rows)
	for r := range data {
		data[r] = make([]byte, cols)
	}
	return matrix{rows: rows, cols: cols, data: data}
}

// vandermonde 生成 rows×cols 的 Vandermonde 矩阵：v[r][c] = exp(r, c)。
func vandermonde(rows, cols int) matrix {
	out := newMatrix(rows, cols)
	for r := 0; r < rows; r++ {
		base := byte(r)
		for c := 0; c < cols; c++ {
			out.data[r][c] = gfPow(base, c)
		}
	}
	return out
}

// subMatrix 从 (row, col) 起取 rows×cols 的子矩阵。
func (m matrix) subMatrix(row, col, rows, cols int) matrix {
	out := newMatrix(rows, cols)
	for r := 0; r < rows; r++ {
		copy(out.data[r], m.data[row+r][col:col+cols])
	}
	return out
}

// multiply 返回 self · other。
func (m matrix) multiply(other matrix) matrix {
	out := newMatrix(m.rows, other.cols)
	for r := 0; r < m.rows; r++ {
		for c := 0; c < other.cols; c++ {
			acc := byte(0)
			for k := 0; k < m.cols; k++ {
				acc ^= gfMul[m.data[r][k]][other.data[k][c]]
			}
			out.data[r][c] = acc
		}
	}
	return out
}

// invert 用高斯-约当消元求逆；GF 上逆矩阵唯一，主元选择不影响结果。
func (m matrix) invert() (matrix, error) {
	if m.rows != m.cols {
		return matrix{}, errors.New("只有方阵可以求逆")
	}
	n := m.rows
	work := newMatrix(n, 2*n)
	for r := 0; r < n; r++ {
		copy(work.data[r], m.data[r])
		work.data[r][n+r] = 1
	}

	for col := 0; col < n; col++ {
		pivot := -1
		for r := col; r < n; r++ {
			if work.data[r][col] != 0 {
				pivot = r
				break
			}
		}
		if pivot < 0 {
			return matrix{}, fmt.Errorf("矩阵不可逆：第 %d 列没有非零主元", col)
		}
		work.data[col], work.data[pivot] = work.data[pivot], work.data[col]

		pivotValue := work.data[col][col]
		if pivotValue != 1 {
			inverse, err := gfInv(pivotValue)
			if err != nil {
				return matrix{}, err
			}
			for c := 0; c < 2*n; c++ {
				work.data[col][c] = gfMul[work.data[col][c]][inverse]
			}
		}

		for r := 0; r < n; r++ {
			if r == col {
				continue
			}
			factor := work.data[r][col]
			if factor == 0 {
				continue
			}
			for c := 0; c < 2*n; c++ {
				work.data[r][c] ^= gfMul[factor][work.data[col][c]]
			}
		}
	}

	out := newMatrix(n, n)
	for r := 0; r < n; r++ {
		copy(out.data[r], work.data[r][n:])
	}
	return out, nil
}

// rsCodec 是一组 (k, m) 的系统码编码器，只保留校验行。
type rsCodec struct {
	k    int
	m    int
	rows [][]byte
}

// newRSCodec 按 Rust crate 的构造方式生成系统码校验行。
func newRSCodec(k, m int) (*rsCodec, error) {
	if k < 1 {
		return nil, errors.New("数据分片数必须 >= 1")
	}
	if m < 1 {
		return nil, errors.New("校验分片数必须 >= 1")
	}
	if k+m > gfOrder {
		return nil, errors.New("分片总数不能超过 256")
	}

	full := vandermonde(k+m, k)
	top := full.subMatrix(0, 0, k, k)
	inverse, err := top.invert()
	if err != nil {
		return nil, fmt.Errorf("构造纠删码矩阵失败: %w", err)
	}
	systematic := full.multiply(inverse)

	rows := make([][]byte, m)
	for i := 0; i < m; i++ {
		rows[i] = systematic.data[k+i]
	}
	return &rsCodec{k: k, m: m, rows: rows}, nil
}

// encode 用前 k 个数据分片覆盖写后 m 个校验分片；入参长度必须正好 k+m。
func (c *rsCodec) encode(shards [][]byte) error {
	if len(shards) != c.k+c.m {
		return fmt.Errorf("需要 %d 个分片，收到 %d 个", c.k+c.m, len(shards))
	}
	size := len(shards[0])
	for i, shard := range shards {
		if len(shard) != size {
			return fmt.Errorf("第 %d 个分片长度 %d，期望 %d", i, len(shard), size)
		}
	}

	for i := 0; i < c.m; i++ {
		row := c.rows[i]
		out := shards[c.k+i]
		for b := range out {
			out[b] = 0
		}
		for j := 0; j < c.k; j++ {
			coef := row[j]
			if coef == 0 {
				continue
			}
			table := &gfMul[coef]
			in := shards[j]
			for b := 0; b < size; b++ {
				out[b] ^= table[in[b]]
			}
		}
	}
	return nil
}
