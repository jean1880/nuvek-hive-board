// Run with `npm test` (node --test; Node strips the TypeScript types natively).
import { test } from 'node:test'
import assert from 'node:assert/strict'
import {
  BULK_STATUSES,
  buildInbox,
  bulkSummary,
  planBulk,
  claimHasMore,
  documentTitle,
  findingTitle,
  formatHash,
  isBulkAllowed,
  needsCount,
  parseHash,
  routeFor,
  statsChanged,
} from './needs.ts'
import type { Finding, Job, Proposal, Stats, Topic } from '../types.ts'

test('title is the first non-empty line with markdown stripped', () => {
  assert.equal(findingTitle('\n\n## **Bold** claim about `code`\nmore detail'), 'Bold claim about code')
  assert.equal(findingTitle('- [link text](http://x.example) here'), 'link text here')
  assert.equal(findingTitle('> quoted *emphasis* line'), 'quoted emphasis line')
  assert.equal(findingTitle('   \n  '), '(untitled)')
})

test('title is capped at 120 characters and never splits a code point', () => {
  const long = findingTitle('a'.repeat(500))
  assert.equal(Array.from(long).length, 120)
  assert.ok(long.endsWith('…'))
  const emoji = findingTitle('🐝'.repeat(300))
  assert.equal(Array.from(emoji).length, 120)
  assert.ok(!emoji.includes('�'))
  assert.equal(findingTitle('x'.repeat(120)), 'x'.repeat(120))
})

test('claimHasMore flags multi-line and truncated claims only', () => {
  assert.equal(claimHasMore('one line', findingTitle('one line')), false)
  assert.equal(claimHasMore('first\nsecond', 'first'), true)
  assert.equal(claimHasMore('a'.repeat(300), findingTitle('a'.repeat(300))), true)
})

test('hash routes round-trip', () => {
  assert.deepEqual(parseHash('#/inbox'), { kind: 'inbox' })
  assert.deepEqual(parseHash('#/topic/ux-frontend'), { kind: 'topic', name: 'ux-frontend' })
  assert.deepEqual(parseHash('#/finding/42'), { kind: 'finding', id: 42 })
  assert.deepEqual(parseHash('#/proposals'), { kind: 'tab', tab: 'proposals' })
  for (const r of [
    { kind: 'inbox' },
    { kind: 'topic', name: 'abc-1' },
    { kind: 'finding', id: 7 },
    { kind: 'tab', tab: 'leaderboard' },
  ] as const) {
    assert.deepEqual(parseHash(formatHash(r)), r)
  }
})

test('malformed hashes are ignored', () => {
  for (const h of ['', '#', '#/', '#/topic', '#/topic/', '#/topic/Bad Name', '#/topic/%E0%A4%A', '#/finding/abc', '#/finding/0', '#/finding/1/2', '#/nope', '#/inbox/x', '#/topic/../x']) {
    assert.equal(parseHash(h), null, h)
  }
})

test('routeFor describes the current view', () => {
  assert.deepEqual(routeFor('inbox', null, null), { kind: 'inbox' })
  assert.deepEqual(routeFor('boards', 'a-b', null), { kind: 'topic', name: 'a-b' })
  assert.deepEqual(routeFor('findings', null, 9), { kind: 'finding', id: 9 })
  assert.deepEqual(routeFor('findings', null, null), { kind: 'tab', tab: 'findings' })
})

const stats = (over: Partial<Stats> = {}): Stats => ({
  active_topics: 1,
  needs_human: 0,
  open_findings: 2,
  ready_jobs: 0,
  stale_jobs: 0,
  total_topics: 3,
  total_messages: 10,
  top_identity: null,
  ...over,
})

test('tab title counts needs-human plus pending proposals, tolerating an old server', () => {
  assert.equal(documentTitle(null), 'Hive')
  assert.equal(documentTitle(stats()), 'Hive')
  assert.equal(documentTitle(stats({ needs_human: 2, proposals_pending: 3 })), '(5) Hive')
  assert.equal(documentTitle(stats({ needs_human: 1 })), '(1) Hive')
  assert.equal(needsCount(stats({ proposals_pending: 4 })), 4)
})

test('statsChanged sees any numeric difference and ignores identical polls', () => {
  assert.equal(statsChanged(null, stats()), true)
  assert.equal(statsChanged(stats(), stats()), false)
  assert.equal(statsChanged(stats(), stats({ total_messages: 11 })), true)
  assert.equal(statsChanged(stats(), stats({ proposals_pending: 1 })), true)
  assert.equal(statsChanged(stats({ proposals_pending: 1 }), stats({ proposals_pending: 1 })), false)
})

test('bulk is limited to non-scoring outcomes', () => {
  assert.deepEqual([...BULK_STATUSES], ['wontfix', 'superseded', 'expired'])
  for (const s of ['wontfix', 'superseded', 'expired']) assert.equal(isBulkAllowed(s), true, s)
  for (const s of ['confirmed', 'fixed', 'false_positive', 'open', '']) assert.equal(isBulkAllowed(s), false, s)
})

test('planBulk applies only findings that are still open', () => {
  const plan = planBulk([1, 2, 3, 4], { 1: 'open', 2: 'confirmed', 3: undefined, 4: 'open' })
  assert.deepEqual(plan, { apply: [1, 4], skip: [2, 3] })
  assert.deepEqual(planBulk([], {}), { apply: [], skip: [] })
})

test('bulkSummary reports applied, skipped and failed', () => {
  assert.equal(bulkSummary(3, 3, 0, 0), '3 of 3 applied')
  assert.equal(bulkSummary(5, 2, 2, 1), '2 of 5 applied, 2 skipped (no longer open), 1 failed')
})

test('inbox collects what needs the operator and nothing else', () => {
  const topic = (name: string, status: string, at: string) => ({ name, status, last_activity_at: at }) as Topic
  const inbox = buildInbox({
    topics: [topic('a', 'needs-human', '2026-01-01'), topic('b', 'active', '2026-01-03'), topic('c', 'needs-human', '2026-01-02')],
    proposals: [{ id: 1, status: 'proposed' }, { id: 2, status: 'approved' }] as Proposal[],
    findings: [
      { id: 1, status: 'open', severity: 'BLOCKER' },
      { id: 2, status: 'open', severity: 'SHOULD' },
      { id: 3, status: 'fixed', severity: 'BLOCKER' },
    ] as Finding[],
    jobs: [{ status: 'done' }, { status: 'verified' }, { status: 'open' }] as Job[],
  })
  assert.deepEqual(inbox.needsHuman.map(t => t.name), ['c', 'a'])
  assert.equal(inbox.proposals.length, 1)
  assert.deepEqual(inbox.blockers.map(f => f.id), [1])
  assert.equal(inbox.unverifiedJobs.length, 1)
  assert.equal(inbox.total, 5)
  assert.equal(buildInbox({ topics: [], proposals: [], findings: [], jobs: [] }).total, 0)
})
