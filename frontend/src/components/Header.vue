<script setup lang="ts">
import { Button } from '@nuvek/ui'
import type { Stats, Tab } from '../types'
import { computed } from 'vue'
import { forgetToken, hasToken, requestToken } from '../lib/auth'
import { needsCount } from '../lib/needs'

const props = defineProps<{
  activeTab: Tab
  stats: Stats | null
  loading: boolean
}>()

defineEmits<{
  (e: 'update:activeTab', tab: Tab): void
  (e: 'openSearch'): void
  (e: 'openCreateTopic'): void
  (e: 'refresh'): void
}>()

// Counts that need the operator; an older server without `proposals_pending` simply shows no badge there.
const badges = computed<Partial<Record<Tab, number>>>(() => ({
  inbox: needsCount(props.stats),
  proposals: props.stats?.proposals_pending ?? 0,
}))

const tabs: { id: Tab; label: string }[] = [
  { id: 'inbox', label: 'Inbox' },
  { id: 'boards', label: 'Boards' },
  { id: 'findings', label: 'Findings' },
  { id: 'leaderboard', label: 'Leaderboard' },
  { id: 'proposals', label: 'Proposals' },
]

const chip = 'px-2 py-1 rounded border'
const chipPlain = `${chip} bg-bg-alt border-border`
const chipWarn = `${chip} bg-warning/10 border-warning/40 text-warning`
const chipLink = 'hover:border-primary/50 focus-visible:outline focus-visible:outline-2 focus-visible:outline-primary'
</script>

<template>
  <header class="border-b border-border bg-surface/80 backdrop-blur sticky top-0 z-30 px-4 lg:px-6 py-3">
    <div class="max-w-7xl mx-auto flex flex-col lg:flex-row items-start lg:items-center justify-between gap-4">
      <div class="flex flex-col sm:flex-row sm:items-center gap-4 sm:gap-6">
        <button
          type="button"
          class="flex items-center gap-2 cursor-pointer select-none text-left"
          @click="$emit('update:activeTab', 'boards')"
        >
          <span class="w-9 h-9 rounded-lg bg-primary/15 border border-primary/30 flex items-center justify-center text-xl" aria-hidden="true">🐝</span>
          <span>
            <span class="block text-base font-bold leading-tight text-fg">Hive Board</span>
            <span class="block text-2xs text-fg-muted font-mono">Agent team knowledge loop</span>
          </span>
        </button>

        <nav aria-label="Sections" class="flex items-center gap-1 bg-bg-alt/80 p-1 rounded-lg border border-border">
          <button
            v-for="tab in tabs"
            :key="tab.id"
            type="button"
            :aria-current="activeTab === tab.id ? 'page' : undefined"
            :class="[
              'px-3 py-1.5 text-xs font-medium rounded-md transition-all cursor-pointer',
              activeTab === tab.id
                ? 'bg-surface text-fg border border-border-strong'
                : 'text-fg-muted hover:text-fg hover:bg-surface/50',
            ]"
            @click="$emit('update:activeTab', tab.id)"
          >
            {{ tab.label }}
            <span
              v-if="badges[tab.id]"
              class="ml-1 px-1.5 py-0.5 rounded-full bg-warning/20 text-warning text-2xs font-mono"
            >{{ badges[tab.id] }}<span class="sr-only"> waiting</span></span>
          </button>
        </nav>
      </div>

      <div class="flex items-center gap-3 w-full lg:w-auto justify-between lg:justify-end flex-wrap">
        <div v-if="stats" class="flex items-center gap-2 text-2xs font-mono text-fg-muted" aria-label="Board statistics">
          <span :class="chipPlain">
            <span class="text-fg font-semibold">{{ stats.active_topics }}</span> active
          </span>
          <a href="#/inbox" :class="[stats.needs_human > 0 ? chipWarn : chipPlain, chipLink]">
            <span class="font-semibold">{{ stats.needs_human }}</span> needs human
          </a>
          <a v-if="stats.proposals_pending" href="#/inbox" :class="[chipWarn, chipLink]">
            <span class="font-semibold">{{ stats.proposals_pending }}</span> proposals pending
          </a>
          <a v-if="stats.open_blockers" href="#/inbox" :class="[chipWarn, chipLink]">
            <span class="font-semibold">{{ stats.open_blockers }}</span> open blockers
          </a>
          <a href="#/findings" :class="[chipPlain, chipLink]">
            <span class="text-fg font-semibold">{{ stats.open_findings }}</span> unjudged findings
          </a>
          <span class="px-2 py-1 rounded bg-bg-alt border border-border">
            <span class="text-fg font-semibold">{{ stats.ready_jobs }}</span> ready jobs
          </span>
          <span v-if="stats.stale_jobs > 0" class="px-2 py-1 rounded border bg-warning/10 border-warning/40 text-warning">
            <span class="font-semibold">{{ stats.stale_jobs }}</span> stale jobs
          </span>
        </div>

        <div class="flex items-center gap-2">
          <Button variant="ghost" class="text-xs px-2.5 py-1.5 border border-border" @click="$emit('openSearch')">
            <span>Search</span>
            <kbd class="text-2xs font-mono px-1 py-0.5 rounded bg-bg-alt border border-border text-fg-dim">Ctrl K</kbd>
          </Button>
          <Button variant="primary" class="text-xs px-3 py-1.5" @click="$emit('openCreateTopic')">
            + New topic
          </Button>
          <Button
            variant="ghost"
            class="text-xs p-1.5 border border-border"
            aria-label="Refresh"
            title="Refresh"
            :disabled="loading"
            @click="$emit('refresh')"
          >
            <span :class="{ 'inline-block animate-spin': loading }" aria-hidden="true">↻</span>
          </Button>
          <Button
            v-if="hasToken"
            variant="ghost"
            class="text-xs px-2.5 py-1.5 border border-border"
            title="Remove the write token from this tab and this device"
            @click="forgetToken()"
          >
            Forget token
          </Button>
          <Button
            v-else
            variant="ghost"
            class="text-xs px-2.5 py-1.5 border border-border"
            title="Enter the write token so changes to the board are accepted"
            @click="requestToken()"
          >
            Set token
          </Button>
        </div>
      </div>
    </div>
  </header>
</template>
