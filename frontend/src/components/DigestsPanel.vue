<script setup lang="ts">
import { onMounted, ref, watch } from 'vue'
import { Chip } from '@nuvek/ui'
import type { Digest, DigestSummary } from '../types'
import { api } from '../api'
import { formatDate } from '../lib/format'
import { renderMarkdown } from '../lib/markdown'

const props = defineProps<{ topic: string }>()
const emit = defineEmits<{ (e: 'error', message: string): void }>()

const digests = ref<DigestSummary[]>([])
const loading = ref(false)
const open = ref<Record<number, Digest | 'loading'>>({})

async function load() {
  loading.value = true
  open.value = {}
  try {
    digests.value = await api.getDigests(props.topic, 50)
  } catch (e) {
    emit('error', `Failed to load digests: ${(e as Error).message}`)
  } finally {
    loading.value = false
  }
}

async function toggle(d: DigestSummary) {
  if (open.value[d.id]) {
    delete open.value[d.id]
    return
  }
  open.value[d.id] = 'loading'
  try {
    open.value[d.id] = await api.getDigest(d.id)
  } catch (e) {
    delete open.value[d.id]
    emit('error', `Failed to load digest: ${(e as Error).message}`)
  }
}

onMounted(load)
watch(() => props.topic, load)
defineExpose({ load })
</script>

<template>
  <div class="space-y-3">
    <div v-if="loading && digests.length === 0" class="text-xs text-fg-dim py-6 text-center">Loading digests...</div>
    <div v-else-if="digests.length === 0" class="text-xs text-fg-dim py-6 text-center">
      No digest yet. The gardener writes one automatically once the topic goes quiet.
    </div>

    <article v-for="d in digests" :key="d.id" class="rounded-lg border border-border bg-surface/90 p-3.5 space-y-2">
      <div class="flex items-center gap-2 flex-wrap">
        <Chip color="running" size="sm">v{{ d.version }}</Chip>
        <Chip color="neutral" variant="outlined" size="sm">{{ d.producer }}</Chip>
        <span class="text-2xs font-mono text-fg-dim">through message #{{ d.through_message_id }}</span>
        <span class="ml-auto text-2xs font-mono text-fg-dim">{{ formatDate(d.created_at) }}</span>
      </div>
      <div class="text-xs text-fg markdown-body" v-html="renderMarkdown(d.summary)"></div>
      <div v-if="d.participants.length" class="text-2xs font-mono text-fg-dim">
        Participants: {{ d.participants.join(', ') }}
      </div>
      <button
        type="button"
        class="text-2xs font-mono text-primary hover:underline cursor-pointer"
        :aria-expanded="!!open[d.id]"
        @click="toggle(d)"
      >
        {{ open[d.id] ? 'Hide transcript' : 'Show transcript' }}
      </button>
      <div v-if="open[d.id] === 'loading'" class="text-2xs text-fg-dim">Loading transcript...</div>
      <div
        v-else-if="open[d.id]"
        class="rounded-md border border-border bg-bg p-3 text-xs markdown-body overflow-x-auto max-h-96 overflow-y-auto"
        v-html="renderMarkdown((open[d.id] as Digest).transcript_md)"
      ></div>
    </article>
  </div>
</template>
