<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import { Button, Chip } from '@nuvek/ui'
import type { Proposal, ProposalStatus } from '../types'
import { api } from '../api'
import { getActor } from '../lib/auth'
import { formatDate, proposalStatusColor } from '../lib/format'
import { renderMarkdown } from '../lib/markdown'

// `version` changes when the live poll saw the board move.
const props = defineProps<{ version?: number }>()

const emit = defineEmits<{
  (e: 'error', message: string): void
  (e: 'notify', message: string): void
  (e: 'openFinding', id: number): void
}>()

const status = ref<'' | ProposalStatus>('proposed')
const proposals = ref<Proposal[]>([])
const loading = ref(false)

async function load() {
  loading.value = true
  try {
    proposals.value = await api.getProposals(status.value || undefined)
  } catch (e) {
    emit('error', `Failed to load proposals: ${(e as Error).message}`)
  } finally {
    loading.value = false
  }
}

async function decide(p: Proposal, to: 'approved' | 'rejected') {
  try {
    await api.decideProposal(p.id, to, getActor())
    emit('notify', `Proposal #${p.id} ${to}`)
    await load()
  } catch (e) {
    emit('error', `Failed to update proposal: ${(e as Error).message}`)
  }
}

onMounted(load)
watch([status, () => props.version], load)

const statuses: ProposalStatus[] = ['proposed', 'approved', 'rejected', 'applied', 'expired']
</script>

<template>
  <div class="flex-1 overflow-y-auto p-4 sm:p-6 lg:p-8 space-y-6 max-w-5xl mx-auto w-full">
    <div class="flex flex-col sm:flex-row items-start sm:items-end justify-between gap-3 border-b border-border pb-4">
      <div>
        <h2 class="text-xl font-bold text-fg">Proposals</h2>
        <p class="text-xs sm:text-sm text-fg-muted max-w-2xl">
          Memories, skills and notes the gardener suggests from repeated, confirmed findings. Nothing is applied until you approve it.
        </p>
      </div>
      <label class="flex items-center gap-1.5 text-2xs font-mono text-fg-dim">
        Status
        <select
          v-model="status"
          class="text-xs px-2 py-1 rounded bg-bg border border-border text-fg font-mono focus:outline-none focus:border-primary/50"
        >
          <option value="">all</option>
          <option v-for="s in statuses" :key="s" :value="s">{{ s }}</option>
        </select>
      </label>
    </div>

    <div v-if="loading && proposals.length === 0" class="text-xs text-fg-dim py-6 text-center">Loading proposals...</div>
    <div v-else-if="proposals.length === 0" class="text-xs text-fg-dim py-6 text-center">No proposals with this status.</div>

    <ul class="space-y-4">
      <li v-for="p in proposals" :key="p.id" class="rounded-lg border border-border bg-surface/90 p-4 space-y-3">
        <div class="flex items-center gap-2 flex-wrap">
          <Chip :color="proposalStatusColor[p.status]" size="sm">{{ p.status }}</Chip>
          <Chip color="neutral" variant="outlined" size="sm">{{ p.kind }}</Chip>
          <h3 class="text-sm font-bold text-fg">{{ p.title }}</h3>
          <span class="ml-auto text-2xs font-mono text-fg-dim">{{ formatDate(p.created_at) }}</span>
        </div>

        <p v-if="p.target" class="text-2xs font-mono text-fg-dim">Target: <code class="text-primary">{{ p.target }}</code></p>

        <div class="text-xs text-fg markdown-body" v-html="renderMarkdown(p.body)"></div>

        <div v-if="p.evidence_finding_ids.length" class="flex items-center gap-1.5 flex-wrap text-2xs font-mono">
          <span class="text-fg-dim">Evidence:</span>
          <button
            v-for="id in p.evidence_finding_ids"
            :key="id"
            type="button"
            class="px-1.5 py-0.5 rounded bg-primary/10 border border-primary/25 text-primary hover:underline cursor-pointer"
            @click="emit('openFinding', id)"
          >
            finding #{{ id }}
          </button>
        </div>

        <div v-if="p.status === 'proposed'" class="flex items-center gap-2 pt-2 border-t border-border/50">
          <Button variant="primary" class="text-xs px-3 py-1.5 normal-case" @click="decide(p, 'approved')">Approve</Button>
          <Button variant="danger" class="text-xs px-3 py-1.5 normal-case" @click="decide(p, 'rejected')">Reject</Button>
        </div>
        <p v-else-if="p.decided_by" class="text-2xs font-mono text-fg-dim">
          Decided by {{ p.decided_by }} {{ formatDate(p.decided_at) }}
        </p>
      </li>
    </ul>
  </div>
</template>
