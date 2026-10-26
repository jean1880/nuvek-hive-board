<script setup lang="ts">
import { computed, nextTick, onBeforeUnmount, onMounted, ref, watch } from 'vue'
import { Button, Chip } from '@nuvek/ui'
import type { Finding, FindingEvent, FindingStatus } from '../types'
import { api } from '../api'
import { getActor } from '../lib/auth'
import { findingStatusColor, formatDate, severityColor, statusLabel } from '../lib/format'
import {
  BULK_STATUSES,
  bulkSummary,
  claimHasMore,
  findingTitle,
  isBulkAllowed,
  planBulk,
  type BulkStatus,
} from '../lib/needs'

// Per-topic when `topic` is set, otherwise the global findings view.
// `version` changes when the live poll saw the board move; `focusId` pins one finding (deep link).
const props = defineProps<{ topic?: string; version?: number; focusId?: number | null }>()

const emit = defineEmits<{
  (e: 'changed'): void
  (e: 'openTopic', name: string): void
  (e: 'error', message: string): void
  (e: 'notify', message: string): void
}>()

const statuses: FindingStatus[] = ['open', 'confirmed', 'fixed', 'false_positive', 'wontfix', 'superseded', 'expired']

const status = ref<'' | FindingStatus>('open')
const severity = ref('')
const kind = ref('')
const findings = ref<Finding[]>([])
const loading = ref(false)
const history = ref<Record<number, FindingEvent[]>>({})
const expanded = ref<number | null>(null)

// Legal finding transitions: open -> any judgement, confirmed -> fixed|wontfix|superseded, any -> open.
const actions: { to: FindingStatus; label: string }[] = [
  { to: 'confirmed', label: 'Confirm' },
  { to: 'fixed', label: 'Fixed' },
  { to: 'false_positive', label: 'False positive' },
  { to: 'wontfix', label: "Won't fix" },
  { to: 'open', label: 'Reopen' },
]

function legal(from: FindingStatus, to: FindingStatus): boolean {
  if (to === 'open') return from !== 'open'
  if (from === 'open') return to !== 'superseded' && to !== 'expired'
  if (from === 'confirmed') return to === 'fixed' || to === 'wontfix'
  return false
}

async function load() {
  loading.value = true
  try {
    const list = await api.getFindings({
      status: status.value || undefined,
      severity: severity.value || undefined,
      kind: kind.value || undefined,
      topic: props.topic,
      limit: 200,
    })
    // A deep-linked finding is shown even when the filters would hide it.
    if (props.focusId != null && !list.some(f => f.id === props.focusId)) {
      try {
        list.unshift(await api.getFinding(props.focusId))
      } catch {
        emit('error', `Finding #${props.focusId} was not found`)
      }
    }
    findings.value = list
    const present = new Set(list.map(f => f.id))
    selected.value = new Set([...selected.value].filter(id => present.has(id)))
    if (props.focusId != null) {
      await nextTick()
      document.getElementById(`finding-${props.focusId}`)?.scrollIntoView({ block: 'center' })
    }
  } catch (e) {
    emit('error', `Failed to load findings: ${(e as Error).message}`)
  } finally {
    loading.value = false
  }
}

async function setStatus(f: Finding, to: FindingStatus) {
  try {
    await api.setFindingStatus(f.id, {
      status: to,
      actor: getActor(),
      actor_class: 'human',
    })
    emit('notify', `Finding #${f.id} marked ${statusLabel(to)}`)
    delete history.value[f.id]
    await load()
    emit('changed')
  } catch (e) {
    emit('error', `Failed to update finding: ${(e as Error).message}`)
  }
}

async function toggleHistory(f: Finding) {
  if (expanded.value === f.id) {
    expanded.value = null
    return
  }
  expanded.value = f.id
  if (!history.value[f.id]) {
    try {
      history.value[f.id] = (await api.getFinding(f.id)).events ?? []
    } catch (e) {
      emit('error', `Failed to load history: ${(e as Error).message}`)
    }
  }
}

// ---- Claim display: first-line title as the heading, the full claim collapsible ----

const claimToggled = ref<Set<number>>(new Set())
const SHORT_CLAIM = 300

function claimOpen(f: Finding): boolean {
  const byDefault = f.claim.length <= SHORT_CLAIM
  return claimToggled.value.has(f.id) ? !byDefault : byDefault
}

function toggleClaim(f: Finding) {
  const next = new Set(claimToggled.value)
  if (next.has(f.id)) next.delete(f.id)
  else next.add(f.id)
  claimToggled.value = next
}

// ---- Triage by exception: bulk to non-scoring outcomes only, with undo ----

const selected = ref<Set<number>>(new Set())
const bulkBusy = ref(false)
const undo = ref<{ ids: number[]; label: string } | null>(null)
let undoTimer: ReturnType<typeof setTimeout> | null = null

const openShown = computed(() => findings.value.filter(f => f.status === 'open'))
const bulkLabels: Record<BulkStatus, string> = { wontfix: "Won't fix", superseded: 'Superseded', expired: 'Expired' }
const bulkHint =
  "Bulk is limited to outcomes that don't feed the leaderboard. Confirmed, fixed and false positive stay one at a time because they drive agent scores. Expired and superseded findings leave the judged denominator."

function toggleSelect(id: number) {
  const next = new Set(selected.value)
  if (next.has(id)) next.delete(id)
  else next.add(id)
  selected.value = next
}

function selectAllOpen() {
  selected.value = new Set(openShown.value.map(f => f.id))
}

function clearUndo() {
  if (undoTimer) clearTimeout(undoTimer)
  undoTimer = null
  undo.value = null
}

/** Sends each PATCH, continuing past errors. Only reopen and the bulk-safe outcomes may be sent. */
async function patchEach(ids: number[], to: FindingStatus): Promise<{ done: number[]; failed: number }> {
  const done: number[] = []
  let failed = 0
  if (to !== 'open' && !isBulkAllowed(to)) return { done, failed: ids.length }
  for (const id of ids) {
    try {
      await api.setFindingStatus(id, { status: to, actor: getActor(), actor_class: 'human' })
      done.push(id)
    } catch (e) {
      failed++
      emit('error', `Finding #${id}: ${(e as Error).message}`)
    }
  }
  return { done, failed }
}

async function bulkApply(to: BulkStatus) {
  const ids = [...selected.value]
  if (!isBulkAllowed(to) || ids.length === 0 || bulkBusy.value) return
  bulkBusy.value = true
  try {
    // Re-read each finding first: one judged by someone else since this list loaded must not be moved.
    const current: Record<number, string | undefined> = {}
    let readFailed = 0
    for (const id of ids) {
      try {
        current[id] = (await api.getFinding(id)).status
      } catch {
        readFailed++
      }
    }
    const { apply, skip } = planBulk(ids, current)
    const sent = await patchEach(apply, to)
    const failed = readFailed + sent.failed
    const skipped = skip.length - readFailed
    selected.value = new Set()
    for (const id of sent.done) delete history.value[id]
    const summary = bulkSummary(ids.length, sent.done.length, skipped, failed)
    if (sent.done.length > 0) {
      clearUndo()
      undo.value = { ids: sent.done, label: `Marked ${statusLabel(to)}: ${summary}` }
      undoTimer = setTimeout(clearUndo, 15000)
    } else {
      emit('notify', summary)
    }
    await load()
    emit('changed')
  } finally {
    bulkBusy.value = false
  }
}

async function undoBulk() {
  const pending = undo.value
  if (!pending) return
  clearUndo()
  bulkBusy.value = true
  try {
    const { done, failed } = await patchEach(pending.ids, 'open')
    for (const id of done) delete history.value[id]
    emit('notify', `${done.length} of ${pending.ids.length} reopened${failed ? `, ${failed} failed` : ''}`)
    await load()
    emit('changed')
  } finally {
    bulkBusy.value = false
  }
}

onMounted(load)
onBeforeUnmount(clearUndo)
watch([status, severity, kind, () => props.topic, () => props.focusId], load)
watch(
  () => props.version,
  () => {
    if (!bulkBusy.value) load()
  },
)

defineExpose({ load })

const selectClass =
  'text-xs px-2 py-1 rounded bg-bg border border-border text-fg font-mono focus:outline-none focus:border-primary/50'
</script>

<template>
  <div class="space-y-4">
    <div class="flex items-center gap-3 flex-wrap">
      <button
        type="button"
        :aria-pressed="status === 'open'"
        title="Findings nobody has judged yet (status open)"
        :class="[
          'text-xs px-2.5 py-1 rounded border font-medium cursor-pointer',
          status === 'open' ? 'bg-primary/20 border-primary/40 text-primary' : 'border-border text-fg-muted hover:text-fg',
        ]"
        @click="status = status === 'open' ? '' : 'open'"
      >
        Unjudged
      </button>
      <label class="flex items-center gap-1.5 text-2xs font-mono text-fg-dim">
        Status
        <select v-model="status" :class="selectClass">
          <option value="">all</option>
          <option v-for="s in statuses" :key="s" :value="s">{{ s === 'open' ? 'open (unjudged)' : statusLabel(s) }}</option>
        </select>
      </label>
      <label class="flex items-center gap-1.5 text-2xs font-mono text-fg-dim">
        Severity
        <select v-model="severity" :class="selectClass">
          <option value="">all</option>
          <option value="BLOCKER">BLOCKER</option>
          <option value="SHOULD">SHOULD</option>
          <option value="NICE">NICE</option>
        </select>
      </label>
      <label class="flex items-center gap-1.5 text-2xs font-mono text-fg-dim">
        Kind
        <select v-model="kind" :class="selectClass">
          <option value="">all</option>
          <option value="finding">finding</option>
          <option value="decision">decision</option>
          <option value="gotcha">gotcha</option>
          <option value="verdict">verdict</option>
          <option value="question">question</option>
        </select>
      </label>
      <span class="text-2xs font-mono text-fg-dim">{{ findings.length }} shown</span>
    </div>

    <div
      v-if="selected.size > 0"
      class="sticky top-16 z-20 flex items-center gap-2 flex-wrap rounded-lg border border-primary/30 bg-surface px-3 py-2"
      role="toolbar"
      aria-label="Bulk actions"
    >
      <span class="text-xs font-semibold text-fg">{{ selected.size }} selected</span>
      <Button
        v-for="s in BULK_STATUSES"
        :key="s"
        variant="ghost"
        class="text-2xs px-2 py-1 border border-border normal-case"
        :title="bulkHint"
        :disabled="bulkBusy"
        @click="bulkApply(s)"
      >
        Mark {{ bulkLabels[s] }}
      </Button>
      <span class="text-2xs text-fg-dim" :title="bulkHint">Why not Confirm or Fixed?</span>
      <button type="button" class="ml-auto text-2xs font-mono text-fg-dim hover:text-fg cursor-pointer" @click="selected = new Set()">
        Clear
      </button>
    </div>
    <div v-else-if="openShown.length > 1" class="text-2xs font-mono">
      <button type="button" class="text-primary hover:underline cursor-pointer" @click="selectAllOpen">
        Select all {{ openShown.length }} open
      </button>
    </div>

    <div v-if="loading && findings.length === 0" class="text-xs text-fg-dim py-6 text-center">Loading findings...</div>
    <div v-else-if="findings.length === 0" class="text-xs text-fg-dim py-6 text-center">No findings match these filters.</div>

    <ul v-else class="space-y-3">
      <li
        v-for="f in findings"
        :id="`finding-${f.id}`"
        :key="f.id"
        :class="[
          'rounded-lg border bg-surface/90 p-3.5 space-y-2',
          focusId === f.id ? 'border-primary ring-1 ring-primary/50' : 'border-border',
        ]"
      >
        <div class="flex items-center gap-1.5 flex-wrap">
          <input
            v-if="f.status === 'open'"
            type="checkbox"
            class="mr-1"
            :checked="selected.has(f.id)"
            :aria-label="`Select finding #${f.id}`"
            @change="toggleSelect(f.id)"
          />
          <Chip :color="findingStatusColor[f.status]" size="sm">{{ statusLabel(f.status) }}</Chip>
          <Chip v-if="f.severity" :color="severityColor[f.severity]" variant="filled" size="sm">{{ f.severity }}</Chip>
          <Chip color="neutral" variant="outlined" size="sm">{{ f.kind }}</Chip>
          <Chip
            v-if="!f.evidence && f.kind !== 'question' && f.kind !== 'decision'"
            color="warning"
            variant="outlined"
            size="sm"
            title="No command output or file:line was attached"
          >
            no evidence
          </Chip>
          <span class="text-2xs font-mono text-fg-dim">#{{ f.id }}</span>
          <span
            v-if="f.reinforce_count > 1"
            class="text-2xs font-mono px-1.5 py-0.5 rounded bg-primary/10 border border-primary/25 text-primary"
            :title="`Reported ${f.reinforce_count} times`"
          >
            reinforced x{{ f.reinforce_count }}
          </span>
          <button
            v-if="!topic"
            type="button"
            class="text-2xs font-mono text-primary hover:underline cursor-pointer"
            @click="emit('openTopic', f.topic_name)"
          >
            {{ f.topic_name }}
          </button>
          <span class="ml-auto text-2xs font-mono text-fg-dim">{{ formatDate(f.updated_at) }}</span>
        </div>

        <h3 class="text-sm font-semibold text-fg break-words">{{ findingTitle(f.claim) }}</h3>

        <template v-if="claimHasMore(f.claim, findingTitle(f.claim))">
          <button
            type="button"
            class="text-2xs font-mono text-fg-dim hover:text-fg cursor-pointer"
            :aria-expanded="claimOpen(f)"
            @click="toggleClaim(f)"
          >
            {{ claimOpen(f) ? 'Hide full claim' : 'Show full claim' }}
          </button>
          <p v-if="claimOpen(f)" class="text-xs text-fg-muted whitespace-pre-wrap break-words">{{ f.claim }}</p>
        </template>

        <div v-if="f.evidence" class="rounded-md border border-border bg-bg p-2.5">
          <div class="text-2xs font-mono uppercase text-fg-dim mb-1">Evidence</div>
          <pre class="text-2xs font-mono text-fg-muted whitespace-pre-wrap break-words">{{ f.evidence }}</pre>
        </div>

        <div v-if="f.subjects.length || f.resolved_subjects.length" class="flex items-center gap-1 flex-wrap text-2xs font-mono">
          <span class="text-fg-dim">Subjects:</span>
          <span v-for="s in f.subjects" :key="`s-${s}`" class="px-1.5 py-0.5 rounded bg-bg border border-border text-fg-muted">{{ s }}</span>
          <template v-if="f.resolved_subjects.length">
            <span class="text-fg-dim ml-1">Resolved:</span>
            <span
              v-for="r in f.resolved_subjects"
              :key="`r-${r.label}-${r.name}`"
              class="px-1.5 py-0.5 rounded bg-success/10 border border-success/30 text-success"
            >
              {{ r.label }}: {{ r.name }}
            </span>
          </template>
        </div>

        <div class="flex items-center gap-2 flex-wrap pt-1 border-t border-border/50">
          <Button
            v-for="a in actions.filter(x => legal(f.status, x.to))"
            :key="a.to"
            variant="ghost"
            class="text-2xs px-2 py-1 border border-border normal-case"
            @click="setStatus(f, a.to)"
          >
            {{ a.label }}
          </Button>
          <button type="button" class="ml-auto text-2xs font-mono text-fg-dim hover:text-fg cursor-pointer" @click="toggleHistory(f)">
            {{ expanded === f.id ? 'Hide history' : 'History' }}
          </button>
        </div>

        <ul v-if="expanded === f.id" class="text-2xs font-mono text-fg-muted space-y-1">
          <li v-if="!history[f.id] || history[f.id].length === 0" class="text-fg-dim">No status changes yet.</li>
          <li v-for="ev in history[f.id]" :key="ev.id">
            {{ formatDate(ev.at) }}: {{ ev.from_status ?? 'new' }} to {{ ev.to_status }} by {{ ev.actor }} ({{ ev.actor_class }})<span v-if="ev.note"> - {{ ev.note }}</span>
          </li>
        </ul>
      </li>
    </ul>

    <div
      v-if="undo"
      class="fixed bottom-4 left-1/2 -translate-x-1/2 z-50 flex items-center gap-3 rounded-lg border border-border-strong bg-surface px-4 py-2.5 shadow-lg"
      role="status"
    >
      <span class="text-xs text-fg">{{ undo.label }}</span>
      <Button variant="ghost" class="text-xs px-2 py-1 border border-border normal-case" @click="undoBulk">Undo</Button>
      <button type="button" class="text-xs text-fg-dim hover:text-fg cursor-pointer" aria-label="Dismiss" @click="clearUndo">✕</button>
    </div>
  </div>
</template>
