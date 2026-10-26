// Pure logic for the live board, the inbox, deep links and bulk triage.
// No Vue and no DOM, so `node --test` can exercise it (see needs.test.ts).
import type { Finding, FindingStatus, Job, Proposal, Stats, Tab, Topic } from '../types.ts'
import { TOPIC_NAME_RE } from '../types.ts'

// ---- Finding title ----

export const TITLE_MAX = 120

/** Strips the inline and leading markdown markers that make a heading line unreadable as plain text. */
function stripMarkdown(line: string): string {
  return line
    .replace(/^\s{0,3}(?:#{1,6}\s+|>+\s?|[-*+]\s+|\d+[.)]\s+)+/, '')
    .replace(/!\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/\[([^\]]*)\]\([^)]*\)/g, '$1')
    .replace(/(\*\*|__)(.+?)\1/g, '$2')
    .replace(/(^|[\s(])[*_]([^*_\s][^*_]*?)[*_](?=$|[\s).,;:!?])/g, '$1$2')
    .replace(/`+([^`]*)`+/g, '$1')
    .replace(/\s+/g, ' ')
    .trim()
}

/**
 * The card heading for a finding: the first non-empty line of the claim with markdown stripped, at most
 * `max` characters. Truncation is by code point, never by UTF-16 unit, so an emoji is not split.
 */
export function findingTitle(claim: string, max: number = TITLE_MAX): string {
  for (const raw of claim.split(/\r?\n/)) {
    const line = stripMarkdown(raw)
    if (!line) continue
    const chars = Array.from(line)
    if (chars.length <= max) return line
    return `${chars
      .slice(0, max - 1)
      .join('')
      .trimEnd()}…`
  }
  return '(untitled)'
}

/** True when the full claim says more than the title does, so the card offers it as a collapsible body. */
export function claimHasMore(claim: string, title: string): boolean {
  return claim.split(/\r?\n/).filter(l => l.trim()).length > 1 || title.endsWith('…')
}

// ---- Hash routes ----

export type Route =
  | { kind: 'inbox' }
  | { kind: 'topic'; name: string }
  | { kind: 'finding'; id: number }
  | { kind: 'tab'; tab: Exclude<Tab, 'inbox'> }

const PLAIN_TABS: ReadonlyArray<Exclude<Tab, 'inbox'>> = ['boards', 'findings', 'leaderboard', 'proposals']

/** Parses `#/inbox`, `#/topic/<name>`, `#/finding/<id>` and `#/<tab>`; anything else is null. */
export function parseHash(hash: string): Route | null {
  const m = /^#\/([a-z]+)(?:\/(.+))?$/.exec(hash)
  if (!m) return null
  const [, head, rest] = m
  if (head === 'inbox' && rest === undefined) return { kind: 'inbox' }
  if (head === 'topic' && rest !== undefined) {
    let name: string
    try {
      name = decodeURIComponent(rest)
    } catch {
      return null
    }
    return TOPIC_NAME_RE.test(name) ? { kind: 'topic', name } : null
  }
  if (head === 'finding' && rest !== undefined && /^[1-9]\d{0,15}$/.test(rest)) {
    return { kind: 'finding', id: Number(rest) }
  }
  const tab = PLAIN_TABS.find(t => t === head)
  if (tab && rest === undefined) return { kind: 'tab', tab }
  return null
}

export function formatHash(route: Route): string {
  switch (route.kind) {
    case 'inbox':
      return '#/inbox'
    case 'topic':
      return `#/topic/${encodeURIComponent(route.name)}`
    case 'finding':
      return `#/finding/${route.id}`
    case 'tab':
      return `#/${route.tab}`
  }
}

/** The route that describes the current view; null for the bare Boards tab with nothing open. */
export function routeFor(tab: Tab, topicName: string | null, findingId: number | null): Route | null {
  if (tab === 'inbox') return { kind: 'inbox' }
  if (tab === 'boards') return topicName ? { kind: 'topic', name: topicName } : { kind: 'tab', tab: 'boards' }
  if (tab === 'findings' && findingId !== null) return { kind: 'finding', id: findingId }
  return { kind: 'tab', tab }
}

// ---- Needs-me count and change detection ----

type StatsLike = Pick<Stats, 'needs_human'> & { proposals_pending?: number | null }

/** needs-human topics plus pending proposals; an older server that omits `proposals_pending` counts 0. */
export function needsCount(stats: StatsLike | null): number {
  if (!stats) return 0
  return (stats.needs_human || 0) + (stats.proposals_pending || 0)
}

export function documentTitle(stats: StatsLike | null): string {
  const n = needsCount(stats)
  return n > 0 ? `(${n}) Hive` : 'Hive'
}

/** True when any numeric stat differs. A missing previous value (first load) counts as a change. */
export function statsChanged(prev: Stats | null, next: Stats): boolean {
  if (!prev) return true
  const a = prev as unknown as Record<string, unknown>
  const b = next as unknown as Record<string, unknown>
  const keys = new Set([...Object.keys(a), ...Object.keys(b)])
  for (const k of keys) {
    if ((typeof a[k] === 'number' || typeof b[k] === 'number') && a[k] !== b[k]) return true
  }
  return false
}

// ---- Bulk triage ----

/**
 * Bulk may only move findings to outcomes that do not feed the quality score. confirmed, fixed and
 * false_positive drive `identity_quality`, so they stay one-at-a-time.
 */
export const BULK_STATUSES = ['wontfix', 'superseded', 'expired'] as const satisfies readonly FindingStatus[]
export type BulkStatus = (typeof BULK_STATUSES)[number]

export function isBulkAllowed(status: string): status is BulkStatus {
  return (BULK_STATUSES as readonly string[]).includes(status)
}

/**
 * Splits a bulk selection by the findings' CURRENT status (read just before sending): only still-open
 * findings are applied, so one judged by someone else in the meantime is never moved out of the score.
 * An id with no known status (fetch failed) is skipped, not guessed.
 */
export function planBulk(
  ids: number[],
  current: Record<number, string | undefined>,
): { apply: number[]; skip: number[] } {
  const apply: number[] = []
  const skip: number[] = []
  for (const id of ids) (current[id] === 'open' ? apply : skip).push(id)
  return { apply, skip }
}

/** "N of M applied, K skipped, E failed" (zero parts omitted, except the applied count). */
export function bulkSummary(total: number, applied: number, skipped: number, failed: number): string {
  const parts = [`${applied} of ${total} applied`]
  if (skipped) parts.push(`${skipped} skipped (no longer open)`)
  if (failed) parts.push(`${failed} failed`)
  return parts.join(', ')
}

// ---- Inbox ----

export interface Inbox {
  needsHuman: Topic[]
  proposals: Proposal[]
  blockers: Finding[]
  unverifiedJobs: Job[]
  total: number
}

export function buildInbox(input: {
  topics: Topic[]
  proposals: Proposal[]
  findings: Finding[]
  jobs: Job[]
}): Inbox {
  const needsHuman = input.topics
    .filter(t => t.status === 'needs-human')
    .sort((a, b) => b.last_activity_at.localeCompare(a.last_activity_at))
  const proposals = input.proposals.filter(p => p.status === 'proposed')
  const blockers = input.findings.filter(f => f.status === 'open' && f.severity === 'BLOCKER')
  const unverifiedJobs = input.jobs.filter(j => j.status === 'done')
  return {
    needsHuman,
    proposals,
    blockers,
    unverifiedJobs,
    total: needsHuman.length + proposals.length + blockers.length + unverifiedJobs.length,
  }
}
