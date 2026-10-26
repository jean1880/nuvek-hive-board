<script setup lang="ts">
import { computed, ref, useId, watch } from 'vue'
import { Button, Chip } from '@nuvek/ui'
import type { Job } from '../types'
import { api } from '../api'
import { getActor } from '../lib/auth'
import { formatDate, identityLabel, jobState, jobStateColor } from '../lib/format'

const props = defineProps<{
  job: Job
  /** Every known job by topic id, to name dependencies. */
  jobsById: Record<number, Job>
}>()

const emit = defineEmits<{
  (e: 'changed'): void
  (e: 'openTopic', name: string): void
  (e: 'error', message: string): void
  (e: 'notify', message: string): void
}>()

type Action = 'reclaim' | 'cancel' | 'retry' | 'amend' | 'priority' | 'verify-pass' | 'verify-fail'

const state = computed(() => jobState(props.job))
const pending = ref<Action | null>(null)
const text = ref('')
const priority = ref(props.job.priority)
const busy = ref(false)
const fieldId = useId()

watch(
  () => props.job.topic_id,
  () => {
    pending.value = null
    text.value = ''
    priority.value = props.job.priority
  },
)

// Legal routes by state (the state x route table in the README's Jobs section); anything else
// would be a 409.
const can = computed(() => {
  const s = props.job.status
  return {
    reclaim: s === 'claimed' || s === 'done',
    cancel: s === 'open' || s === 'claimed' || s === 'done' || s === 'failed',
    retry: s === 'failed' || s === 'cancelled',
    amend: true,
    priority: s === 'open' || s === 'claimed' || s === 'done',
    verify: s === 'done',
  }
})

const needsText: Record<Action, 'none' | 'optional' | 'required' | 'number'> = {
  reclaim: 'optional',
  cancel: 'optional',
  retry: 'none',
  amend: 'required',
  priority: 'number',
  'verify-pass': 'optional',
  'verify-fail': 'required',
}

const labels: Record<Action, string> = {
  reclaim: 'Reclaim',
  cancel: 'Cancel job',
  retry: 'Retry',
  amend: 'Append amendment',
  priority: 'Set priority',
  'verify-pass': 'Verify: pass',
  'verify-fail': 'Verify: fail',
}

function start(a: Action) {
  text.value = ''
  priority.value = props.job.priority
  if (needsText[a] === 'none') {
    pending.value = a
    run()
    return
  }
  pending.value = a
}

const canSubmit = computed(() => {
  if (!pending.value) return false
  const n = needsText[pending.value]
  if (n === 'required') return text.value.trim() !== ''
  if (n === 'number') return Number.isInteger(priority.value)
  return true
})

async function run() {
  const a = pending.value
  if (!a || busy.value) return
  const name = props.job.topic_name
  const actor = getActor()
  const note = text.value.trim()
  busy.value = true
  try {
    if (a === 'reclaim') await api.reclaimJob(name, actor, note)
    else if (a === 'cancel') await api.cancelJob(name, actor, note)
    else if (a === 'retry') await api.retryJob(name, actor)
    else if (a === 'amend') await api.amendJob(name, note, actor)
    else if (a === 'priority') await api.setJobPriority(name, Number(priority.value), actor)
    else await api.verifyJob(name, a === 'verify-pass' ? 'pass' : 'fail', note, actor)
    emit('notify', `${labels[a]}: done`)
    pending.value = null
    text.value = ''
    emit('changed')
  } catch (e) {
    emit('error', `${labels[a]} failed: ${(e as Error).message}`)
  } finally {
    busy.value = false
  }
}

function depLabel(id: number): string {
  const j = props.jobsById[id]
  return j ? j.title : `job #${id}`
}

function scrollToMessage(id?: number | null) {
  if (id) document.getElementById(`message-${id}`)?.scrollIntoView({ behavior: 'smooth', block: 'center' })
}

const fieldClass =
  'text-xs px-2.5 py-1.5 rounded-md bg-bg border border-border text-fg placeholder:text-fg-dim focus:outline-none focus:border-primary/50'
</script>

<template>
  <section
    :class="[
      'rounded-lg border bg-surface/70 p-4 space-y-3',
      state === 'blocked' || state === 'failed' ? 'border-error/50' : state === 'stale' ? 'border-warning/50' : 'border-border-strong',
    ]"
    aria-label="Job"
  >
    <div class="flex items-center gap-1.5 flex-wrap">
      <Chip :color="jobStateColor[state]" variant="filled" size="sm">{{ state }}</Chip>
      <Chip v-if="job.stale" color="warning" variant="outlined" size="sm">stale</Chip>
      <Chip color="neutral" variant="outlined" size="sm">{{ job.capability }}</Chip>
      <Chip v-if="job.security" color="error" variant="outlined" size="sm" title="Verification needs a different harness">security</Chip>
      <Chip v-if="job.task_kind" color="neutral" variant="outlined" size="sm">{{ job.task_kind }}</Chip>
      <span class="text-2xs font-mono text-fg-muted">priority {{ job.priority }}</span>
      <span class="text-2xs font-mono text-fg-muted">round {{ job.round }}</span>
      <span class="text-2xs font-mono" :class="job.verify_fails > 0 ? 'text-warning' : 'text-fg-muted'">
        verify fails {{ job.verify_fails }}
      </span>
      <span class="text-2xs font-mono text-fg-muted">attempts {{ job.attempts }}</span>
    </div>

    <div
      v-if="job.blocked"
      class="rounded-md border border-error/40 bg-error/10 p-2.5 text-xs text-error"
      role="alert"
    >
      <span class="font-semibold">Blocked:</span> {{ job.blocked_reason ?? 'a dependency failed or was cancelled' }}
    </div>
    <div
      v-else-if="job.status === 'open' && !job.ready && job.blocked_by.length"
      class="rounded-md border border-border bg-bg p-2.5 text-xs text-fg-muted"
    >
      Waiting on {{ job.blocked_by.map(depLabel).join(', ') }}
    </div>
    <div
      v-if="job.conflicts_with.length"
      class="rounded-md border border-warning/40 bg-warning/10 p-2.5 text-xs text-warning"
    >
      Owned paths overlap with {{ job.conflicts_with.map(depLabel).join(', ') }}
    </div>
    <div v-if="job.stale" class="rounded-md border border-warning/40 bg-warning/10 p-2.5 text-xs text-warning">
      Claimed by {{ job.claimed_by }} for {{ Math.round((job.claim_age_s ?? 0) / 3600) }} h with no recent activity. Reclaim it if the worker is gone.
    </div>

    <dl class="grid grid-cols-1 sm:grid-cols-2 gap-x-6 gap-y-2 text-2xs font-mono">
      <div v-if="job.depends_on.length" class="sm:col-span-2">
        <dt class="text-fg-dim uppercase">Depends on</dt>
        <dd class="flex flex-wrap gap-1 mt-0.5">
          <button
            v-for="id in job.depends_on"
            :key="id"
            type="button"
            :class="[
              'px-1.5 py-0.5 rounded border cursor-pointer hover:underline',
              job.blocked_by.includes(id) ? 'border-error/50 bg-error/10 text-error' : 'border-border bg-bg text-fg-muted',
            ]"
            :title="job.blocked_by.includes(id) ? 'Not done yet' : 'Satisfied'"
            @click="jobsById[id] && emit('openTopic', jobsById[id].topic_name)"
          >
            {{ depLabel(id) }}<span v-if="job.blocked_by.includes(id)"> (blocking)</span>
          </button>
        </dd>
      </div>

      <div v-if="job.owns.length" class="sm:col-span-2">
        <dt class="text-fg-dim uppercase">Owns</dt>
        <dd class="mt-0.5 space-y-0.5 text-fg-muted break-all">
          <div v-for="p in job.owns" :key="p">{{ p }}</div>
        </dd>
      </div>

      <div v-if="job.claimed_by">
        <dt class="text-fg-dim uppercase">Claimed by</dt>
        <dd class="text-fg-muted">{{ job.claimed_by }} · {{ formatDate(job.claimed_at) }}</dd>
      </div>

      <div v-if="job.worker_identity || job.worker_identity_id">
        <dt class="text-fg-dim uppercase">Worker identity</dt>
        <dd class="text-fg-muted">
          {{ job.worker_identity ? identityLabel(job.worker_identity) : `identity #${job.worker_identity_id}` }}
        </dd>
      </div>

      <div v-if="job.result_message_id">
        <dt class="text-fg-dim uppercase">Result</dt>
        <dd>
          <button type="button" class="text-primary hover:underline cursor-pointer" @click="scrollToMessage(job.result_message_id)">
            message #{{ job.result_message_id }}
          </button>
          <span class="text-fg-dim"> · {{ formatDate(job.completed_at) }}</span>
        </dd>
      </div>

      <div v-if="job.verdict_message_id">
        <dt class="text-fg-dim uppercase">Verdict</dt>
        <dd>
          <button type="button" class="text-primary hover:underline cursor-pointer" @click="scrollToMessage(job.verdict_message_id)">
            message #{{ job.verdict_message_id }}
          </button>
          <span v-if="job.verified_at" class="text-fg-dim"> · verified {{ formatDate(job.verified_at) }}</span>
        </dd>
      </div>

      <div v-if="job.tier || job.agent_type">
        <dt class="text-fg-dim uppercase">Suggested worker</dt>
        <dd class="text-fg-muted">{{ [job.agent_type, job.tier].filter(Boolean).join(' / ') }}</dd>
      </div>
    </dl>

    <details class="text-xs">
      <summary class="cursor-pointer text-fg-muted hover:text-fg">Spec</summary>
      <pre class="mt-2 whitespace-pre-wrap break-words rounded-md border border-border bg-bg p-3 font-mono text-2xs text-fg-muted">{{ job.spec }}</pre>
    </details>

    <div class="flex items-center gap-2 flex-wrap pt-2 border-t border-border/50">
      <Button v-if="can.verify" variant="primary" class="text-2xs px-2.5 py-1 normal-case" @click="start('verify-pass')">Verify: pass</Button>
      <Button v-if="can.verify" variant="danger" class="text-2xs px-2.5 py-1 normal-case" @click="start('verify-fail')">Verify: fail</Button>
      <Button v-if="can.reclaim" variant="ghost" class="text-2xs px-2.5 py-1 border border-border normal-case" @click="start('reclaim')">Reclaim</Button>
      <Button v-if="can.retry" variant="ghost" class="text-2xs px-2.5 py-1 border border-border normal-case" @click="start('retry')">Retry</Button>
      <Button v-if="can.priority" variant="ghost" class="text-2xs px-2.5 py-1 border border-border normal-case" @click="start('priority')">Reprioritise</Button>
      <Button variant="ghost" class="text-2xs px-2.5 py-1 border border-border normal-case" @click="start('amend')">Amend</Button>
      <Button v-if="can.cancel" variant="ghost" class="text-2xs px-2.5 py-1 border border-border normal-case hover:text-error" @click="start('cancel')">Cancel job</Button>
    </div>

    <form v-if="pending && needsText[pending] !== 'none'" class="flex items-end gap-2 flex-wrap" @submit.prevent="run">
      <label :for="fieldId" class="flex-1 min-w-48 text-2xs font-mono text-fg-dim">
        {{ labels[pending] }}
        <span v-if="pending === 'verify-pass' || pending === 'verify-fail'"> (as human / human / human)</span>
        <input
          v-if="pending === 'priority'"
          :id="fieldId"
          v-model.number="priority"
          type="number"
          step="1"
          :class="[fieldClass, 'block w-full mt-1 font-mono']"
        />
        <textarea
          v-else-if="pending === 'amend'"
          :id="fieldId"
          v-model="text"
          rows="3"
          placeholder="Text appended to the job as an amendment (the spec itself stays unchanged)"
          :class="[fieldClass, 'block w-full mt-1 resize-y']"
        ></textarea>
        <input
          v-else
          :id="fieldId"
          v-model="text"
          type="text"
          :placeholder="needsText[pending] === 'required' ? 'Note (required)' : 'Note (optional)'"
          :class="[fieldClass, 'block w-full mt-1']"
        />
      </label>
      <Button variant="primary" type="submit" class="text-xs px-3 py-1.5 normal-case" :disabled="!canSubmit || busy">Confirm</Button>
      <Button variant="ghost" type="button" class="text-xs normal-case" @click="pending = null">Cancel</Button>
    </form>
  </section>
</template>
