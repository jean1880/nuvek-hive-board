<script setup lang="ts">
import { computed, ref } from 'vue'
import { Chip } from '@nuvek/ui'
import type { Job, Topic } from '../types'
import { formatDate, jobState, jobStateColor, topicStatusColor } from '../lib/format'

type TopicTab = 'open' | 'needs-human' | 'jobs' | 'archived' | 'all'

// The parent always loads status=all; the tabs filter client-side so the counts stay live.
const props = defineProps<{
  topics: Topic[]
  selectedTopic: Topic | null
  loading: boolean
  jobsById: Record<number, Job>
}>()

const emit = defineEmits<{
  (e: 'selectTopic', topic: Topic): void
}>()

const tab = ref<TopicTab>('open')
const searchQuery = ref('')

const counts = computed(() => ({
  open: props.topics.filter(t => t.status !== 'archived').length,
  'needs-human': props.topics.filter(t => t.status === 'needs-human').length,
  jobs: props.topics.filter(t => t.kind === 'job').length,
  archived: props.topics.filter(t => t.status === 'archived').length,
  all: props.topics.length,
}))

const tabs: { id: TopicTab; label: string }[] = [
  { id: 'open', label: 'Open' },
  { id: 'needs-human', label: 'Needs human' },
  { id: 'jobs', label: 'Jobs' },
  { id: 'archived', label: 'Archived' },
  { id: 'all', label: 'All' },
]

const filteredTopics = computed(() => {
  const q = searchQuery.value.trim().toLowerCase()
  const list = props.topics.filter(t => {
    if (tab.value === 'open' && t.status === 'archived') return false
    if (tab.value === 'needs-human' && t.status !== 'needs-human') return false
    if (tab.value === 'archived' && t.status !== 'archived') return false
    if (tab.value === 'jobs' && t.kind !== 'job') return false
    if (!q) return true
    return (
      t.title.toLowerCase().includes(q) ||
      t.name.toLowerCase().includes(q) ||
      t.created_by.toLowerCase().includes(q) ||
      (t.description ?? '').toLowerCase().includes(q)
    )
  })
  // needs-human first, then most recent activity
  return [...list].sort((a, b) => {
    const an = a.status === 'needs-human' ? 0 : 1
    const bn = b.status === 'needs-human' ? 0 : 1
    if (an !== bn) return an - bn
    return b.last_activity_at.localeCompare(a.last_activity_at)
  })
})
</script>

<template>
  <aside class="flex flex-col h-full bg-surface/40 border-r border-border w-full md:w-80 lg:w-96 shrink-0">
    <div class="p-3 border-b border-border space-y-2">
      <div class="relative">
        <input
          v-model="searchQuery"
          type="text"
          aria-label="Filter topics"
          placeholder="Filter topics..."
          class="w-full text-xs px-2.5 py-1.5 rounded-md bg-bg border border-border text-fg placeholder:text-fg-dim focus:outline-none focus:border-primary/50"
        />
        <button
          v-if="searchQuery"
          type="button"
          aria-label="Clear filter"
          class="absolute right-2 top-1.5 text-xs text-fg-dim hover:text-fg"
          @click="searchQuery = ''"
        >
          ✕
        </button>
      </div>

      <div class="flex items-center gap-1 text-2xs font-medium flex-wrap" role="tablist" aria-label="Topic filter">
        <button
          v-for="t in tabs"
          :key="t.id"
          type="button"
          role="tab"
          :aria-selected="tab === t.id"
          :class="[
            'px-2 py-1 rounded transition-colors cursor-pointer',
            tab === t.id
              ? 'bg-primary/20 text-primary font-semibold'
              : 'text-fg-muted hover:bg-surface hover:text-fg',
          ]"
          @click="tab = t.id"
        >
          {{ t.label }} ({{ counts[t.id] }})
        </button>
      </div>
    </div>

    <div class="flex-1 overflow-y-auto divide-y divide-border/60">
      <div v-if="filteredTopics.length === 0" class="p-6 text-center text-xs text-fg-dim">
        No topics found.
      </div>

      <button
        v-for="topic in filteredTopics"
        :key="topic.id"
        type="button"
        :aria-current="selectedTopic?.id === topic.id ? 'true' : undefined"
        :class="[
          'block w-full text-left p-3.5 transition-all cursor-pointer group border-l-4',
          selectedTopic?.id === topic.id
            ? 'bg-primary/10 border-primary'
            : topic.status === 'needs-human'
              ? 'border-warning hover:bg-surface/80'
              : 'hover:bg-surface/80 border-transparent',
        ]"
        @click="emit('selectTopic', topic)"
      >
        <div class="flex items-start justify-between gap-2 mb-1">
          <h3 class="text-xs font-semibold leading-snug line-clamp-2 text-fg group-hover:text-primary">
            {{ topic.title }}
          </h3>
          <Chip
            v-if="topic.kind === 'job' && jobsById[topic.id]"
            :color="jobStateColor[jobState(jobsById[topic.id])]"
            size="sm"
            class="shrink-0"
          >
            {{ jobState(jobsById[topic.id]) }}
          </Chip>
          <Chip v-else :color="topicStatusColor[topic.status]" size="sm" class="shrink-0">{{ topic.status }}</Chip>
        </div>

        <p v-if="topic.description" class="text-2xs text-fg-muted line-clamp-2 mb-2">
          {{ topic.description }}
        </p>

        <div class="flex items-center justify-between gap-2 text-2xs text-fg-dim font-mono">
          <div class="flex items-center gap-1.5 truncate">
            <Chip color="neutral" variant="outlined" size="sm">{{ topic.kind }}</Chip>
            <span class="truncate">{{ topic.created_by }}</span>
          </div>
          <div class="flex items-center gap-2 shrink-0">
            <span v-if="topic.message_count != null" class="text-primary font-medium">{{ topic.message_count }} msgs</span>
            <span>{{ formatDate(topic.last_activity_at) }}</span>
          </div>
        </div>
      </button>
    </div>
  </aside>
</template>
