import type {
  ActorClass,
  AliasScore,
  Digest,
  DigestSummary,
  Finding,
  FindingStatus,
  IdentityQuality,
  Job,
  JobVerdict,
  Message,
  PostMessagePayload,
  PostMessageResponse,
  Proposal,
  ProposalStatus,
  ScoreResult,
  SearchResponse,
  Stats,
  ThreadNode,
  Topic,
  TopicEvent,
  TopicFilter,
  TopicKind,
  TopicStatus,
} from './types'
import { HUMAN_IDENTITY } from './types'
import { forgetToken, getToken, requestToken } from './lib/auth'

const BASE_URL = './api'

async function send(path: string, options: RequestInit, token?: string): Promise<Response> {
  const headers: Record<string, string> = { 'Content-Type': 'application/json' }
  if (token) headers.Authorization = `Bearer ${token}`
  return fetch(`${BASE_URL}${path}`, { ...options, headers })
}

async function fail(res: Response): Promise<never> {
  let msg = `Request failed: ${res.statusText} (${res.status})`
  try {
    const data = await res.json()
    if (data?.error) msg = data.error
  } catch {
    // keep the status line
  }
  throw new Error(msg)
}

// Set when the operator dismisses the token prompt for a read, so background refreshes do not
// reopen it on every poll. Cleared as soon as a token is entered (for a write or a read).
let readPromptDeclined = false

/**
 * Reads carry the token when one is known: the server gates reads by default
 * (`HIVE_AUTH_READS`). A 401 opens the token modal once and retries with what was typed.
 */
async function read<T>(path: string): Promise<T> {
  let res = await send(path, { method: 'GET' }, getToken() || undefined)
  if (res.status === 401 && !readPromptDeclined) {
    forgetToken()
    const token = await requestToken()
    if (!token) {
      readPromptDeclined = true
      throw new Error('The board needs the API token to show its contents.')
    }
    res = await send(path, { method: 'GET' }, token)
  }
  if (!res.ok) return fail(res)
  readPromptDeclined = false
  return res.json()
}

/**
 * Writes carry the bearer token. With no token yet, or after a 401, the token modal is
 * opened and the write is retried once with what was typed.
 */
async function write<T>(method: string, path: string, body?: unknown): Promise<T> {
  const opts: RequestInit = {
    method,
    body: body === undefined ? undefined : JSON.stringify(body),
  }
  let token = getToken() || (await requestToken())
  if (!token) throw new Error('A write token is required for this action.')
  readPromptDeclined = false
  let res = await send(path, opts, token)
  if (res.status === 401) {
    forgetToken()
    token = await requestToken()
    if (!token) throw new Error('The write token was not accepted.')
    res = await send(path, opts, token)
  }
  if (!res.ok) return fail(res)
  if (res.status === 204) return undefined as T
  return res.json()
}

function qs(params: Record<string, string | number | boolean | undefined | null>): string {
  const p = new URLSearchParams()
  for (const [k, v] of Object.entries(params)) {
    if (v !== undefined && v !== null && v !== '') p.set(k, String(v))
  }
  const s = p.toString()
  return s ? `?${s}` : ''
}

const t = encodeURIComponent

export const api = {
  getStats: () => read<Stats>('/stats'),

  getTopics: (status: TopicFilter = 'open', kind?: TopicKind) =>
    read<Topic[]>(`/topics${qs({ status, kind })}`),

  getTopic: (topic: string) => read<Topic>(`/topics/${t(topic)}`),

  createTopic: (payload: {
    name: string
    title: string
    description?: string
    author: string
    kind?: TopicKind
  }) => write<Topic>('POST', '/topics', payload),

  updateTopic: (
    topic: string,
    payload: {
      title?: string
      description?: string
      status?: TopicStatus
      outcome?: string
      actor: string
    },
  ) => write<Topic>('PATCH', `/topics/${t(topic)}`, payload),

  /** Hard delete (human-only). The server requires ?confirm=<topic name>. */
  deleteTopic: (topic: string, confirm: string) =>
    write<void>('DELETE', `/topics/${t(topic)}${qs({ confirm })}`),

  getMessages: (topic: string, options?: { parent_id?: number; limit?: number }) =>
    read<Message[]>(`/topics/${t(topic)}/messages${qs({ ...options })}`),

  getThreads: (topic: string) => read<ThreadNode[]>(`/topics/${t(topic)}/threads`),

  getTopicEvents: (topic: string) => read<TopicEvent[]>(`/topics/${t(topic)}/events`),

  postMessage: (topic: string, payload: PostMessagePayload) =>
    write<PostMessageResponse>('POST', `/topics/${t(topic)}/messages`, payload),

  deleteMessage: (id: number) => write<void>('DELETE', `/messages/${id}`),

  getFindings: (filter?: {
    status?: FindingStatus
    severity?: string
    kind?: string
    topic?: string
    subject?: string
    limit?: number
  }) => read<Finding[]>(`/findings${qs({ ...filter })}`),

  getFinding: (id: number) => read<Finding>(`/findings/${id}`),

  setFindingStatus: (
    id: number,
    payload: { status: FindingStatus; note?: string; actor: string; actor_class: ActorClass },
  ) => write<Finding>('PATCH', `/findings/${id}`, payload),

  getJobs: (filter?: { status?: string; ready?: boolean; blocked?: boolean; limit?: number }) =>
    read<Job[]>(`/jobs${qs({ ...filter })}`),

  getJob: (topic: string) => read<Job>(`/jobs/${t(topic)}`),

  amendJob: (topic: string, text: string, author: string) =>
    write<Message>('POST', `/jobs/${t(topic)}/amend`, { author, agent: HUMAN_IDENTITY, text }),

  reclaimJob: (topic: string, actor: string, reason: string) =>
    write<Job>('POST', `/jobs/${t(topic)}/reclaim`, { actor, reason }),

  cancelJob: (topic: string, actor: string, reason: string) =>
    write<Job>('POST', `/jobs/${t(topic)}/cancel`, { actor, reason }),

  retryJob: (topic: string, actor: string) => write<Job>('POST', `/jobs/${t(topic)}/retry`, { actor }),

  setJobPriority: (topic: string, priority: number, actor: string) =>
    write<Job>('PATCH', `/jobs/${t(topic)}`, { priority, actor }),

  /** The verifier is the human operator (human/human/human), never derived from the author alias. */
  verifyJob: (topic: string, verdict: JobVerdict, note: string, author: string) =>
    write<Job>('POST', `/jobs/${t(topic)}/verify`, {
      verdict,
      note,
      author,
      verifier: HUMAN_IDENTITY,
      agent: HUMAN_IDENTITY,
    }),

  getDigests: (topic?: string, limit?: number) =>
    read<DigestSummary[]>(`/digests${qs({ topic, limit })}`),

  getDigest: (id: number) => read<Digest>(`/digests/${id}`),

  /** Server-side keyword fallback digest; the gardener produces the full one. */
  distillKeyword: (topic: string) =>
    write<Digest>('POST', `/topics/${t(topic)}/digest`, { producer: 'keyword' }),

  getLeaderboard: (limit?: number) => read<IdentityQuality[]>(`/leaderboard${qs({ limit })}`),

  getAliasLeaderboard: (limit?: number) =>
    read<AliasScore[]>(`/leaderboard/aliases${qs({ limit })}`),

  scoreAgent: (payload: {
    agent_name: string
    score: number
    reason: string
    topic?: string
    award_top_contributor?: boolean
  }) => write<ScoreResult>('POST', '/score', payload),

  getProposals: (status?: ProposalStatus) => read<Proposal[]>(`/proposals${qs({ status })}`),

  decideProposal: (id: number, status: 'approved' | 'rejected', actor: string) =>
    write<Proposal>('PATCH', `/proposals/${id}`, { status, actor }),

  search: (options: { q: string; topic?: string; author?: string; limit?: number }) =>
    read<SearchResponse>(`/search${qs({ ...options })}`),
}
