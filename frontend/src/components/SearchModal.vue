<script setup lang="ts">
import { ref, watch } from 'vue'
import { Chip, Modal } from '@nuvek/ui'
import type { SearchKind, SearchResponse, SearchResult } from '../types'
import type { ChipColor } from '../lib/format'
import { api } from '../api'

const kindColor: Record<SearchKind, ChipColor> = {
  topic: 'running',
  message: 'neutral',
  finding: 'warning',
  digest: 'success',
}

const props = defineProps<{
  open: boolean
}>()

const emit = defineEmits<{
  (e: 'close'): void
  (e: 'selectResult', result: SearchResult): void
}>()

const query = ref('')
const searchResponse = ref<SearchResponse | null>(null)
const loading = ref(false)

let debounceTimer: ReturnType<typeof setTimeout> | null = null

watch(query, (val) => {
  if (debounceTimer) clearTimeout(debounceTimer)
  if (!val.trim()) {
    searchResponse.value = null
    return
  }
  debounceTimer = setTimeout(async () => {
    loading.value = true
    try {
      searchResponse.value = await api.search({ q: val.trim() })
    } catch (e) {
      console.error('Search failed', e)
    } finally {
      loading.value = false
    }
  }, 200)
})

function onSelect(r: SearchResult) {
  emit('selectResult', r)
  emit('close')
}
</script>

<template>
  <Modal :open="open" title="Global Search" @close="emit('close')">
    <div class="space-y-4 pt-1">
      <!-- Search Input -->
      <div class="relative">
        <input
          v-model="query"
          type="text"
          aria-label="Search"
          placeholder="Search topics, messages, findings, digests..."
          class="w-full text-sm px-3.5 py-2.5 rounded-lg bg-bg border border-border text-fg placeholder:text-fg-dim focus:outline-none focus:border-primary/70"
          autofocus
        />
        <span v-if="loading" class="absolute right-3 top-2.5 text-xs text-fg-dim animate-spin">
          🔄
        </span>
      </div>

      <!-- Suggestions if any -->
      <div
        v-if="searchResponse?.suggestions && searchResponse.suggestions.length > 0"
        class="flex items-center gap-1.5 flex-wrap text-2xs font-mono"
      >
        <span class="text-fg-dim">Did you mean:</span>
        <button
          v-for="s in searchResponse.suggestions"
          :key="s"
          type="button"
          class="px-2 py-0.5 rounded bg-surface border border-border text-primary hover:bg-surface-hover cursor-pointer"
          @click="query = s"
        >
          {{ s }}
        </button>
      </div>

      <!-- Results list -->
      <div class="max-h-96 overflow-y-auto divide-y divide-border/60 -mx-4 px-4">
        <div
          v-if="searchResponse && searchResponse.results.length === 0"
          class="py-8 text-center text-xs text-fg-dim"
        >
          No results found for "{{ searchResponse.query }}"
        </div>

        <div
          v-for="res in searchResponse?.results"
          :key="`${res.kind}-${res.id}`"
          class="py-3 px-2 rounded-lg hover:bg-surface transition-all cursor-pointer group"
          role="button"
          tabindex="0"
          @click="onSelect(res)"
          @keydown.enter="onSelect(res)"
        >
          <div class="flex items-center justify-between gap-2 mb-1">
            <div class="flex items-center gap-2 truncate">
              <Chip :color="kindColor[res.kind] ?? 'neutral'" size="sm">{{ res.kind }}</Chip>
              <span class="text-xs font-semibold text-fg group-hover:text-primary truncate">
                {{ res.topic_name }}
              </span>
            </div>
            <span class="text-2xs font-mono text-fg-dim shrink-0">
              Score: {{ (res.score * 100).toFixed(0) }}%
            </span>
          </div>

          <p class="text-xs text-fg-muted font-sans line-clamp-2 mb-1">
            {{ res.title_or_snippet }}
          </p>

          <div class="flex items-center gap-2 text-2xs font-mono text-fg-dim">
            <span>By <strong class="text-fg">{{ res.author }}</strong></span>
            <span>#{{ res.id }}</span>
          </div>
        </div>
      </div>
    </div>
  </Modal>
</template>
