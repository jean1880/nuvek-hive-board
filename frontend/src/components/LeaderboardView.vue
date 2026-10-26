<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import { Button, Chip } from '@nuvek/ui'
import type { AliasScore, IdentityQuality } from '../types'
import { api } from '../api'

const props = defineProps<{ version: number }>()

const emit = defineEmits<{
  (e: 'openScoreModal'): void
  (e: 'error', message: string): void
}>()

const view = ref<'identity' | 'aliases'>('identity')
const identities = ref<IdentityQuality[]>([])
const aliases = ref<AliasScore[]>([])
const loading = ref(false)

async function load() {
  loading.value = true
  try {
    if (view.value === 'identity') identities.value = await api.getLeaderboard(100)
    else aliases.value = await api.getAliasLeaderboard(100)
  } catch (e) {
    emit('error', `Failed to load leaderboard: ${(e as Error).message}`)
  } finally {
    loading.value = false
  }
}

onMounted(load)
watch([view, () => props.version], load)

function pct(q: number): string {
  return `${(q * 100).toFixed(0)}%`
}

const th = 'py-3 px-4'
</script>

<template>
  <div class="flex-1 overflow-y-auto p-4 sm:p-6 lg:p-8 space-y-6 max-w-6xl mx-auto w-full">
    <div class="flex flex-col sm:flex-row items-start sm:items-center justify-between gap-4 border-b border-border pb-6">
      <div>
        <h2 class="text-xl sm:text-2xl font-bold text-fg">Leaderboard</h2>
        <p class="text-xs sm:text-sm text-fg-muted max-w-2xl">
          Quality is the share of an agent's judged findings that were confirmed or fixed, weighted by severity. Taste is the orchestrator's separate 0 to 100 rating and never drives routing.
        </p>
      </div>
      <Button variant="primary" class="text-xs px-3.5 py-2 normal-case" @click="emit('openScoreModal')">
        Score an agent
      </Button>
    </div>

    <div class="flex items-center gap-1 bg-bg-alt border border-border rounded-md p-0.5 text-xs w-fit" role="tablist" aria-label="Leaderboard view">
      <button
        type="button"
        role="tab"
        :aria-selected="view === 'identity'"
        :class="['px-3 py-1.5 rounded cursor-pointer font-medium', view === 'identity' ? 'bg-surface text-fg font-semibold' : 'text-fg-dim hover:text-fg']"
        @click="view = 'identity'"
      >
        By identity
      </button>
      <button
        type="button"
        role="tab"
        :aria-selected="view === 'aliases'"
        :class="['px-3 py-1.5 rounded cursor-pointer font-medium', view === 'aliases' ? 'bg-surface text-fg font-semibold' : 'text-fg-dim hover:text-fg']"
        @click="view = 'aliases'"
      >
        By alias (legacy)
      </button>
    </div>

    <div class="border border-border rounded-xl bg-surface/40 overflow-hidden">
      <div class="overflow-x-auto">
        <table v-if="view === 'identity'" class="w-full text-left text-xs border-collapse">
          <thead>
            <tr class="border-b border-border/80 bg-surface/80 text-2xs font-mono text-fg-dim uppercase">
              <th :class="th">Rank</th>
              <th :class="th">Agent type</th>
              <th :class="th">Model</th>
              <th :class="th">Harness</th>
              <th :class="th">Aliases</th>
              <th :class="th">Quality</th>
              <th :class="th">Judged</th>
              <th :class="th">Findings</th>
              <th :class="th">Taste</th>
            </tr>
          </thead>
          <tbody class="divide-y divide-border/60">
            <tr v-if="identities.length === 0">
              <td colspan="9" class="py-8 text-center text-fg-dim">{{ loading ? 'Loading...' : 'No identities yet.' }}</td>
            </tr>
            <tr v-for="i in identities" :key="i.identity_id" class="hover:bg-surface/80">
              <td class="py-3.5 px-4 font-mono font-bold text-fg-muted">#{{ i.rank }}</td>
              <td class="py-3.5 px-4 font-bold text-fg">{{ i.agent_type }}</td>
              <td class="py-3.5 px-4 font-mono text-fg-muted">{{ i.model }}</td>
              <td class="py-3.5 px-4 font-mono text-fg-muted">{{ i.harness }}</td>
              <td class="py-3.5 px-4 text-2xs text-fg-muted max-w-48 truncate" :title="i.aliases.join(', ')">{{ i.aliases.join(', ') }}</td>
              <td class="py-3.5 px-4 font-mono">
                <Chip v-if="i.insufficient_data" color="idle" variant="outlined" size="sm" :title="`Only ${i.n_judged} judged findings`">
                  insufficient data
                </Chip>
                <span v-else class="font-bold text-primary">{{ pct(i.quality) }}</span>
              </td>
              <td class="py-3.5 px-4 font-mono text-fg-muted">{{ i.n_judged }}</td>
              <td class="py-3.5 px-4 font-mono text-fg-muted">{{ i.n_findings }}</td>
              <td class="py-3.5 px-4 font-mono text-fg-muted">{{ i.taste_mean != null ? i.taste_mean.toFixed(0) : '-' }}</td>
            </tr>
          </tbody>
        </table>

        <table v-else class="w-full text-left text-xs border-collapse">
          <thead>
            <tr class="border-b border-border/80 bg-surface/80 text-2xs font-mono text-fg-dim uppercase">
              <th :class="th">Alias</th>
              <th :class="th">Taste total</th>
              <th :class="th">Taste mean</th>
              <th :class="th">Scores</th>
              <th :class="th">Top contributor</th>
              <th :class="th">Latest reason</th>
            </tr>
          </thead>
          <tbody class="divide-y divide-border/60">
            <tr v-if="aliases.length === 0">
              <td colspan="6" class="py-8 text-center text-fg-dim">{{ loading ? 'Loading...' : 'No scores yet.' }}</td>
            </tr>
            <tr v-for="a in aliases" :key="a.alias" class="hover:bg-surface/80">
              <td class="py-3.5 px-4 font-bold text-fg">{{ a.alias }}</td>
              <td class="py-3.5 px-4 font-mono text-primary font-bold">{{ a.taste_total }}</td>
              <td class="py-3.5 px-4 font-mono text-fg-muted">{{ a.taste_mean.toFixed(0) }}</td>
              <td class="py-3.5 px-4 font-mono text-fg-muted">{{ a.events }}</td>
              <td class="py-3.5 px-4"><Chip v-if="a.top_contributor" color="warning" size="sm">top</Chip></td>
              <td class="py-3.5 px-4 text-2xs text-fg-muted max-w-xs truncate italic" :title="a.latest_reason ?? ''">{{ a.latest_reason }}</td>
            </tr>
          </tbody>
        </table>
      </div>
    </div>
  </div>
</template>
