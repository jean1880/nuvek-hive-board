<script setup lang="ts">
import { computed, onMounted, ref, watch } from 'vue'
import { Chip } from '@nuvek/ui'
import type { Finding, Job, Proposal, Topic } from '../types'
import { api } from '../api'
import { formatDate, severityColor } from '../lib/format'
import { buildInbox, findingTitle, formatHash } from '../lib/needs'

// Topics and jobs arrive already loaded by the app; proposals and BLOCKER findings are fetched here.
// `version` changes when the live poll saw the board move.
// `jobsError` is set by the app when the last jobs fetch failed (the list shown is then the last good one).
// `blockerTotal` is stats.open_blockers, when the server reports it.
const props = defineProps<{
  topics: Topic[]
  jobs: Job[]
  version?: number
  jobsError?: boolean
  blockerTotal?: number
}>()

const emit = defineEmits<{ (e: 'error', message: string): void; (e: 'retry'): void }>()

const BLOCKER_LIMIT = 50
const proposals = ref<Proposal[]>([])
const findings = ref<Finding[]>([])
const loaded = ref(false)
const proposalsFailed = ref(false)
const blockersFailed = ref(false)

// Each source fails on its own; a failed one keeps its last good list and is named in the error state.
async function load() {
  const [p, f] = await Promise.allSettled([
    api.getProposals('proposed'),
    api.getFindings({ status: 'open', severity: 'BLOCKER', limit: BLOCKER_LIMIT }),
  ])
  proposalsFailed.value = p.status === 'rejected'
  blockersFailed.value = f.status === 'rejected'
  if (p.status === 'fulfilled') proposals.value = p.value
  if (f.status === 'fulfilled') findings.value = f.value
  loaded.value = true
}

function retry() {
  emit('retry')
  void load()
}

onMounted(load)
watch(() => props.version, load)

const failedSources = computed(() => {
  const out: string[] = []
  if (proposalsFailed.value) out.push('proposals')
  if (blockersFailed.value) out.push('open blockers')
  if (props.jobsError) out.push('jobs')
  return out
})

const inbox = computed(() =>
  buildInbox({ topics: props.topics, proposals: proposals.value, findings: findings.value, jobs: props.jobs }),
)

const blockersHidden = computed(() => Math.max(0, (props.blockerTotal ?? 0) - inbox.value.blockers.length))

const itemClass =
  'block rounded-lg border border-border bg-surface/90 p-3.5 space-y-1.5 hover:border-primary/50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-primary'
</script>

<template>
  <div class="flex-1 overflow-y-auto p-4 sm:p-6 lg:p-8 space-y-6 max-w-4xl mx-auto w-full">
    <div class="border-b border-border pb-4">
      <h2 class="text-xl font-bold text-fg">Inbox</h2>
      <p class="text-xs sm:text-sm text-fg-muted max-w-2xl">What needs a human decision right now.</p>
    </div>

    <div
      v-if="loaded && failedSources.length"
      role="alert"
      class="flex items-center gap-3 flex-wrap rounded-lg border border-warning/40 bg-warning/10 px-3.5 py-2.5 text-xs text-warning"
    >
      <span>Couldn't load: {{ failedSources.join(', ') }}. What is shown may be incomplete.</span>
      <button type="button" class="ml-auto font-semibold underline cursor-pointer" @click="retry">Retry</button>
    </div>

    <div v-if="!loaded" class="text-xs text-fg-dim py-6 text-center">Loading...</div>

    <div v-else-if="inbox.total === 0 && failedSources.length === 0" class="text-center py-16 space-y-1">
      <p class="text-sm font-semibold text-fg">Nothing needs you</p>
      <p class="text-xs text-fg-dim">No topics are waiting on a human, no proposals to decide, no open blockers, no jobs to verify.</p>
    </div>

    <template v-else>
      <section v-if="inbox.needsHuman.length" aria-labelledby="inbox-topics" class="space-y-2">
        <h3 id="inbox-topics" class="text-2xs font-mono uppercase text-fg-dim">Waiting on a human ({{ inbox.needsHuman.length }})</h3>
        <a
          v-for="t in inbox.needsHuman"
          :key="t.id"
          :href="formatHash({ kind: 'topic', name: t.name })"
          :class="itemClass"
        >
          <div class="flex items-center gap-2 flex-wrap">
            <Chip color="warning" size="sm">needs-human</Chip>
            <span class="text-sm font-semibold text-fg">{{ t.title }}</span>
            <span class="ml-auto text-2xs font-mono text-fg-dim">{{ formatDate(t.last_activity_at) }}</span>
          </div>
          <p v-if="t.description" class="text-xs text-fg-muted line-clamp-3 whitespace-pre-wrap">{{ t.description }}</p>
        </a>
      </section>

      <section v-if="inbox.proposals.length" aria-labelledby="inbox-proposals" class="space-y-2">
        <h3 id="inbox-proposals" class="text-2xs font-mono uppercase text-fg-dim">Proposals to decide ({{ inbox.proposals.length }})</h3>
        <a v-for="p in inbox.proposals" :key="p.id" :href="formatHash({ kind: 'tab', tab: 'proposals' })" :class="itemClass">
          <div class="flex items-center gap-2 flex-wrap">
            <Chip color="warning" size="sm">proposed</Chip>
            <Chip color="neutral" variant="outlined" size="sm">{{ p.kind }}</Chip>
            <span class="text-sm font-semibold text-fg">{{ p.title }}</span>
            <span class="ml-auto text-2xs font-mono text-fg-dim">{{ formatDate(p.created_at) }}</span>
          </div>
        </a>
      </section>

      <section v-if="inbox.blockers.length" aria-labelledby="inbox-blockers" class="space-y-2">
        <h3 id="inbox-blockers" class="text-2xs font-mono uppercase text-fg-dim">Open blockers ({{ inbox.blockers.length }})</h3>
        <a v-for="f in inbox.blockers" :key="f.id" :href="formatHash({ kind: 'finding', id: f.id })" :class="itemClass">
          <div class="flex items-center gap-2 flex-wrap">
            <Chip :color="severityColor.BLOCKER" variant="filled" size="sm">BLOCKER</Chip>
            <span class="text-sm font-semibold text-fg break-words">{{ findingTitle(f.claim) }}</span>
          </div>
          <div class="text-2xs font-mono text-fg-dim">#{{ f.id }} in {{ f.topic_name }}</div>
        </a>
        <p v-if="blockersHidden > 0" class="text-2xs font-mono text-fg-dim">
          Showing {{ inbox.blockers.length }} of {{ blockerTotal }} —
          <a href="#/findings" class="text-primary hover:underline">open Findings</a>
        </p>
      </section>

      <section v-if="inbox.unverifiedJobs.length" aria-labelledby="inbox-jobs" class="space-y-2">
        <h3 id="inbox-jobs" class="text-2xs font-mono uppercase text-fg-dim">Done, awaiting verification ({{ inbox.unverifiedJobs.length }})</h3>
        <a
          v-for="j in inbox.unverifiedJobs"
          :key="j.topic_id"
          :href="formatHash({ kind: 'topic', name: j.topic_name })"
          :class="itemClass"
        >
          <div class="flex items-center gap-2 flex-wrap">
            <Chip color="warning" size="sm">done</Chip>
            <span class="text-sm font-semibold text-fg">{{ j.title }}</span>
            <span class="ml-auto text-2xs font-mono text-fg-dim">{{ formatDate(j.completed_at) }}</span>
          </div>
        </a>
      </section>
    </template>
  </div>
</template>
