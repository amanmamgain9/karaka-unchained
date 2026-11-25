// Writes a small synthetic v4 session the way dsh's JSONL backend does:
// one checksummed zstd frame for the header, then one per batch, made with
// the same Node API and options as dsh's `compressZstdFrame`
// (zlib.zstdCompress with ZSTD_c_checksumFlag = 1, default level).
import { constants, zstdCompressSync } from 'node:zlib'
import { writeFileSync } from 'node:fs'

const frame = text => zstdCompressSync(text, { params: { [constants.ZSTD_c_checksumFlag]: 1 } })
const lines = rows => rows.map(row => JSON.stringify(row)).join('\n') + '\n'

const header = { type: 'session', version: 4, id: 'dsh-written', createdAt: 1000, cwd: '/work', isSeeded: false, delegationDepth: 0 }
const firstTurn = [
  { type: 'turn/start', seq: 0, time: 1, data: { turn: 1 } },
  { type: 'user/message', seq: 1, time: 2, data: { id: 'one-turn-user', role: 'user', content: [{ type: 'text', text: 'hi' }], source: { kind: 'user' } }, surfaceOp: 'append' },
  { type: 'step/start', seq: 2, time: 3, data: { turn: 1, step: 1 } },
  { type: 'assistant/message', seq: 3, time: 4, data: {
    turn: 1, step: 1,
    message: { id: 'one-turn-assistant', role: 'assistant', content: [{ type: 'text', text: 'hello' }], source: { kind: 'model', provider: 'mock', model: 'mock' } },
    stream: [
      { type: 'chunk', time: 3, chunk: { type: 'block-start', index: 0, blockType: 'text' } },
      { type: 'text-chunks', time0: 3, index: 0, dt: [], texts: ['hello'] },
      { type: 'chunk', time: 4, chunk: { type: 'block-end', index: 0, block: { type: 'text', text: 'hello' } } },
      { type: 'chunk', time: 4, chunk: { type: 'finish', reason: { kind: 'stop' } } },
    ],
  }, surfaceOp: 'append' },
  { type: 'step/end', seq: 4, time: 5, data: { turn: 1, step: 1 } },
  { type: 'turn/end', seq: 5, time: 6, data: { turn: 1, reason: { kind: 'completed' } } },
]
const secondTurn = [
  { type: 'turn/start', seq: 6, time: 9, data: { turn: 2 } },
  { type: 'turn/end', seq: 7, time: 10, data: { turn: 2, reason: { kind: 'completed' } } },
]
writeFileSync(process.argv[2], Buffer.concat([frame(lines([header])), frame(lines(firstTurn)), frame(lines(secondTurn))]))
