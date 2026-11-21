package main

import (
	"fmt"
	"sync"

	"screentools/client-go/screencode"
)

// maxCachedFrames 是帧缓存上限，满了整表清空（与 Rust 侧一致）。
const maxCachedFrames = 4

// player 持有一份文件的编码结果，负责按帧号渲染 PNG。
type player struct {
	header        screencode.Header
	geometry      screencode.Geometry
	shards        [][]byte
	slotsPerFrame int
	frameCount    int
	source        string
	fileLen       uint64
	dwellMs       uint64
	hashHex       string

	mu    sync.Mutex
	cache map[int][]byte
}

// newPlayer 按分片数与版式推导总帧数。
func newPlayer(
	header screencode.Header,
	geometry screencode.Geometry,
	shards [][]byte,
	source string,
	fileLen uint64,
	dwellMs uint64,
	hashHex string,
) *player {
	slotsPerFrame := geometry.SlotsPerFrame()
	frameCount := (int(header.TotalShards) + slotsPerFrame - 1) / slotsPerFrame
	return &player{
		header:        header,
		geometry:      geometry,
		shards:        shards,
		slotsPerFrame: slotsPerFrame,
		frameCount:    frameCount,
		source:        source,
		fileLen:       fileLen,
		dwellMs:       dwellMs,
		hashHex:       hashHex,
		cache:         make(map[int][]byte),
	}
}

// bytesPerFrame 是一帧占用的码制字节数（含 slot 头尾）。
func (p *player) bytesPerFrame() int {
	return p.slotsPerFrame * p.header.SlotBytes()
}

// framePNG 生成第 index 帧（0 基）的 PNG。
func (p *player) framePNG(index int) ([]byte, error) {
	if index >= p.frameCount || index < 0 {
		return nil, fmt.Errorf("帧号 %d 超出范围 1..=%d", index+1, p.frameCount)
	}

	p.mu.Lock()
	if cached, ok := p.cache[index]; ok {
		p.mu.Unlock()
		return cached, nil
	}
	p.mu.Unlock()

	start := index * p.slotsPerFrame
	end := start + p.slotsPerFrame
	if end > len(p.shards) {
		end = len(p.shards)
	}
	slots := make([][]byte, 0, end-start)
	for i := start; i < end; i++ {
		slots = append(slots, screencode.EncodeSlot(uint32(i), p.shards[i]))
	}

	png, err := screencode.RenderPNG(p.geometry, p.header, slots)
	if err != nil {
		return nil, err
	}

	p.mu.Lock()
	if len(p.cache) >= maxCachedFrames {
		p.cache = make(map[int][]byte)
	}
	p.cache[index] = png
	p.mu.Unlock()
	return png, nil
}
