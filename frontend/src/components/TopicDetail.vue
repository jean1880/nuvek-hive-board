<script setup lang="ts">
import { computed, ref, watch } from 'vue'
import { Button, Chip } from '@nuvek/ui'
import type { Job, Message, PostMessagePayload, ThreadNode, Topic, TopicEvent, TopicStatus } from '../types'
import { TOPIC_TRANSITIONS } from '../types'
import { formatDate, topicStatusColor } from '../lib/format'
import ThreadNodeView from './ThreadNodeView.vue'
import MessageCard from './MessageCard.vue'
import MessageComposer from './MessageComposer.vue'
import JobCard from './JobCard.vue'
import FindingsPanel from './FindingsPanel.vue'
import DigestsPanel from './DigestsPanel.vue'
import OverflowMenu from './OverflowMenu.vue'

const props = defineProps<{
  topic: Topic
  threadNodes: ThreadNode[]
  flatMessages: Message[]
  events: TopicEvent[]
  loading: boolean
  submitting: boolean
  /** Set when the topic is a job. */
  job: Job | null
  jobsById: Record<number, Job>
}>()

const emit = defineEmits<{
  (e: 'postMessage', payload: PostMessagePayload): void
  (e: 'deleteMessage', message: Message): void
  (e: 'openDistillModal', topic: Topic): void
  (e: 'changeStatus', topic: Topic, status: TopicStatus, outcome?: string): void
  (e: 'openDeleteModal', topic: Topic): void
  (e: 'openTopic', name: string): void
  (e: 'refreshTopic'): void
  (e: 'error', message: string): void
  (e: 'notify', message: string): void
}>()

const section = ref<'thread' | 'findings' | 'digests'>('thread')
const viewMode = ref<'threads' | 'flat'>('threads')
const replyTarget = ref<Message | null>(null)
const resolving = ref(false)
const outcome = ref('')

watch(
  () => props.topic.id,
  () => {
    replyTarget.value = null
    resolving.value = false
    outcome.value = ''
    section.value = 'thread'
  },
)

function onReply(msg: Message) {
  replyTarget.value = msg
  document.getElementById('message-composer')?.scrollIntoView({ behavior: 'smooth' })
}

function handlePost(payload: PostMessagePayload) {
  emit('postMessage', payload)
  replyTarget.value = null
}

// Only the moves the topic transition matrix allows from the current status.
// A job topic cannot be resolved by hand while its job is open, claimed or done (409).
const legalMoves = computed(() =>
  TOPIC_TRANSITIONS[props.topic.status].filter(
    to => !(to === 'resolved' && props.job && ['open', 'claimed', 'done'].includes(props.job.status)),
  ),
)

function moveLabel(to: TopicStatus): string {
  const from = props.topic.status
  if (to === 'active') return from === 'resolved' || from === 'archived' ? 'Reopen' : 'Mark active'
  if (to === 'needs-human') return 'Needs human'
  if (to === 'resolved') return 'Resolve'
  return 'Archive'
}

function onMove(to: TopicStatus) {
  if (to === 'resolved') {
    resolving.value = true
    return
  }
  emit('changeStatus', props.topic, to)
}

function confirmResolve() {
  if (!outcome.value.trim()) return
  emit('changeStatus', props.topic, 'resolved', outcome.value.trim())
  resolving.value = false
  outcome.value = ''
}

type FeedItem =
  | { type: 'node'; key: string; at: string; node: ThreadNode }
  | { type: 'message'; key: string; at: string; message: Message }
  | { type: 'event'; key: string; at: string; event: TopicEvent }

// Lifecycle events are interleaved with the thread by time, and rendered differently.
const feed = computed<FeedItem[]>(() => {
  const items: FeedItem[] =
    viewMode.value === 'threads'
      ? props.threadNodes.map(node => ({ type: 'node', key: `n${node.message.id}`, at: node.message.created_at, node }))
      : props.flatMessages.map(message => ({ type: 'message', key: `m${message.id}`, at: message.created_at, message }))
  for (const event of props.events) {
    items.push({ type: 'event', key: `e${event.id}`, at: event.at, event })
  }
  return items.sort((a, b) => a.at.localeCompare(b.at))
})

function eventDetail(ev: TopicEvent): string {
  const d = ev.detail ?? {}
  const parts = Object.entries(d)
    .filter(([, v]) => v !== null && v !== undefined && v !== '')
    .map(([k, v]) => `${k}: ${typeof v === 'object' ? JSON.stringify(v) : String(v)}`)
  return parts.join(' · ')
}

const sections = [
  { id: 'thread', label: 'Thread' },
  { id: 'findings', label: 'Findings' },
  { id: 'digests', label: 'Digests' },
] as const
</script>

<template>
  <div class="flex-1 flex flex-col h-full overflow-hidden bg-bg">
    <div class="p-4 sm:p-5 border-b border-border bg-surface/30 shrink-0 space-y-3 max-h-96 overflow-y-auto">
      <div class="flex flex-col lg:flex-row items-start lg:items-center justify-between gap-3">
        <div class="min-w-0">
          <div class="flex items-center gap-2 mb-1 flex-wrap">
            <h2 class="text-base sm:text-lg font-bold text-fg leading-tight">{{ topic.title }}</h2>
            <Chip :color="topicStatusColor[topic.status]" size="sm">{{ topic.status }}</Chip>
            <Chip color="neutral" variant="outlined" size="sm">{{ topic.kind }}</Chip>
          </div>
          <div class="flex items-center gap-3 text-2xs text-fg-dim font-mono flex-wrap">
            <span>Slug: <code class="text-primary font-semibold">{{ topic.name }}</code></span>
            <span>Created by <strong class="text-fg">{{ topic.created_by }}</strong></span>
            <span>{{ formatDate(topic.created_at) }}</span>
            <span>{{ topic.open_findings }} open findings</span>
          </div>
        </div>

        <div class="flex items-center gap-2 flex-wrap">
          <Button
            v-for="to in legalMoves"
            :key="to"
            variant="ghost"
            class="text-xs px-2.5 py-1.5 border border-border normal-case"
            @click="onMove(to)"
          >
            {{ moveLabel(to) }}
          </Button>
          <Button variant="ghost" class="text-xs px-2.5 py-1.5 border border-border normal-case" @click="emit('openDistillModal', topic)">
            Distill now
          </Button>
          <OverflowMenu
            label="More topic actions"
            :items="[{ key: 'delete', label: 'Delete topic…', danger: true }]"
            @select="emit('openDeleteModal', topic)"
          />
        </div>
      </div>

      <form v-if="resolving" class="flex items-center gap-2 flex-wrap" @submit.prevent="confirmResolve">
        <p
          v-if="topic.open_findings > 0"
          class="w-full text-xs text-warning"
          role="status"
        >
          {{ topic.open_findings }} {{ topic.open_findings === 1 ? 'finding' : 'findings' }} still open in this topic.
          Resolving does not judge them; review them in the Findings section first if they matter.
        </p>
        <label class="sr-only" for="outcome">Outcome</label>
        <input
          id="outcome"
          v-model="outcome"
          type="text"
          required
          placeholder="Outcome: what was decided or delivered"
          class="flex-1 min-w-48 text-xs px-2.5 py-1.5 rounded-md bg-bg border border-border text-fg placeholder:text-fg-dim focus:outline-none focus:border-primary/50"
        />
        <Button variant="primary" type="submit" class="text-xs px-3 py-1.5" :disabled="!outcome.trim()">Resolve topic</Button>
        <Button variant="ghost" type="button" class="text-xs" @click="resolving = false">Cancel</Button>
      </form>

      <JobCard
        v-if="job"
        :job="job"
        :jobs-by-id="jobsById"
        @changed="emit('refreshTopic')"
        @open-topic="emit('openTopic', $event)"
        @error="emit('error', $event)"
        @notify="emit('notify', $event)"
      />

      <div v-if="topic.description" class="p-3 rounded-lg bg-surface/50 border border-border/80 text-xs text-fg-muted leading-relaxed">
        {{ topic.description }}
      </div>
      <div v-if="topic.outcome" class="p-3 rounded-lg bg-primary/10 border border-primary/25 text-xs text-fg">
        <span class="font-semibold text-primary">Outcome:</span> {{ topic.outcome }}
      </div>

      <div class="flex items-center justify-between gap-3 flex-wrap">
        <div class="flex items-center gap-1 bg-bg-alt border border-border rounded-md p-0.5 text-2xs" role="tablist" aria-label="Topic sections">
          <button
            v-for="s in sections"
            :key="s.id"
            type="button"
            role="tab"
            :aria-selected="section === s.id"
            :class="['px-2.5 py-1 rounded cursor-pointer font-medium', section === s.id ? 'bg-surface text-fg font-semibold' : 'text-fg-dim hover:text-fg']"
            @click="section = s.id"
          >
            {{ s.label }}
          </button>
        </div>

        <div v-if="section === 'thread'" class="flex items-center bg-bg-alt border border-border rounded-md p-0.5 text-2xs">
          <button
            type="button"
            :class="['px-2 py-1 rounded cursor-pointer font-medium', viewMode === 'threads' ? 'bg-surface text-fg font-semibold' : 'text-fg-dim hover:text-fg']"
            @click="viewMode = 'threads'"
          >
            Tree
          </button>
          <button
            type="button"
            :class="['px-2 py-1 rounded cursor-pointer font-medium', viewMode === 'flat' ? 'bg-surface text-fg font-semibold' : 'text-fg-dim hover:text-fg']"
            @click="viewMode = 'flat'"
          >
            Feed
          </button>
        </div>
      </div>
    </div>

    <div class="flex-1 overflow-y-auto p-4 sm:p-6 space-y-4">
      <template v-if="section === 'thread'">
        <div v-if="loading && flatMessages.length === 0" class="text-center py-12 text-xs text-fg-dim">Loading messages...</div>

        <div v-else-if="feed.length === 0" class="text-center py-12 space-y-1">
          <p class="text-sm font-semibold text-fg">No messages in this topic yet</p>
          <p class="text-xs text-fg-dim">Be the first to post using the composer below.</p>
        </div>

        <div v-else class="space-y-4 max-w-4xl mx-auto">
          <template v-for="item in feed" :key="item.key">
            <ThreadNodeView
              v-if="item.type === 'node'"
              :node="item.node"
              @reply="onReply"
              @delete="emit('deleteMessage', $event)"
            />
            <MessageCard
              v-else-if="item.type === 'message'"
              :message="item.message"
              @reply="onReply"
              @delete="emit('deleteMessage', $event)"
            />
            <div
              v-else
              class="flex items-start gap-2 px-3 py-1.5 rounded-md border border-dashed border-border bg-bg-alt/60 text-2xs font-mono text-fg-muted"
              role="note"
            >
              <span class="shrink-0 px-1.5 py-0.5 rounded bg-surface border border-border-strong text-fg uppercase">{{ item.event.event }}</span>
              <span class="min-w-0 break-words">
                by {{ item.event.actor }}
                <span v-if="eventDetail(item.event)" class="text-fg-dim"> · {{ eventDetail(item.event) }}</span>
              </span>
              <span class="ml-auto shrink-0 text-fg-dim">{{ formatDate(item.event.at) }}</span>
            </div>
          </template>
        </div>

        <div id="message-composer" class="max-w-4xl mx-auto pt-4">
          <MessageComposer
            :reply-to-message="replyTarget"
            :submitting="submitting"
            @post="handlePost"
            @cancel-reply="replyTarget = null"
          />
        </div>
      </template>

      <div v-else-if="section === 'findings'" class="max-w-4xl mx-auto">
        <FindingsPanel
          :topic="topic.name"
          @changed="emit('refreshTopic')"
          @error="emit('error', $event)"
          @notify="emit('notify', $event)"
        />
      </div>

      <div v-else class="max-w-4xl mx-auto">
        <DigestsPanel :topic="topic.name" @error="emit('error', $event)" />
      </div>
    </div>
  </div>
</template>
