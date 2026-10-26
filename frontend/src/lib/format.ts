import type { Job, FindingStatus, MessageKind, ProposalStatus, Severity, TopicStatus } from '../types'

export type ChipColor = 'success' | 'error' | 'warning' | 'running' | 'idle' | 'neutral'

export function formatDate(iso?: string | null): string {
  if (!iso) return ''
  const d = new Date(iso)
  if (Number.isNaN(d.getTime())) return iso
  return d.toLocaleString(undefined, {
    month: 'short',
    day: 'numeric',
    hour: '2-digit',
    minute: '2-digit',
  })
}

export const topicStatusColor: Record<TopicStatus, ChipColor> = {
  active: 'success',
  quiet: 'idle',
  'needs-human': 'warning',
  resolved: 'running',
  archived: 'neutral',
}

export const findingStatusColor: Record<FindingStatus, ChipColor> = {
  open: 'warning',
  confirmed: 'running',
  fixed: 'success',
  false_positive: 'neutral',
  wontfix: 'idle',
  superseded: 'idle',
  expired: 'neutral',
}

export const severityColor: Record<Severity, ChipColor> = {
  BLOCKER: 'error',
  SHOULD: 'warning',
  NICE: 'neutral',
}

export const proposalStatusColor: Record<ProposalStatus, ChipColor> = {
  proposed: 'warning',
  approved: 'running',
  rejected: 'neutral',
  applied: 'success',
  expired: 'idle',
}

export function messageKindColor(kind: MessageKind): ChipColor {
  switch (kind) {
    case 'finding':
    case 'gotcha':
      return 'warning'
    case 'decision':
    case 'verdict':
    case 'result':
      return 'running'
    case 'amendment':
      return 'warning'
    case 'question':
      return 'idle'
    default:
      return 'neutral'
  }
}

export function statusLabel(s: string): string {
  return s.replace(/_/g, ' ')
}

export function splitList(raw: string): string[] {
  return raw
    .split(',')
    .map(s => s.trim())
    .filter(Boolean)
}

export type JobState =
  | 'ready'
  | 'waiting'
  | 'blocked'
  | 'claimed'
  | 'done'
  | 'verified'
  | 'failed'
  | 'cancelled'
  | 'stale'

/** The one chip a job shows: stale beats claimed; ready/blocked/waiting refine `open`. */
export function jobState(job: Job): JobState {
  if (job.stale) return 'stale'
  if (job.status === 'open') {
    if (job.ready) return 'ready'
    return job.blocked ? 'blocked' : 'waiting'
  }
  return job.status
}

export const jobStateColor: Record<JobState, ChipColor> = {
  ready: 'success',
  waiting: 'idle',
  blocked: 'error',
  claimed: 'running',
  done: 'warning',
  verified: 'success',
  failed: 'error',
  cancelled: 'neutral',
  stale: 'warning',
}

export function identityLabel(i?: { agent_type: string; model: string; harness: string } | null): string {
  return i ? `${i.agent_type} / ${i.model} / ${i.harness}` : ''
}
