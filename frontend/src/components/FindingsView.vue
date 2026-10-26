<script setup lang="ts">
import FindingsPanel from './FindingsPanel.vue'

defineProps<{ version?: number; focusId?: number | null }>()

const emit = defineEmits<{
  (e: 'changed'): void
  (e: 'openTopic', name: string): void
  (e: 'error', message: string): void
  (e: 'notify', message: string): void
}>()
</script>

<template>
  <div class="flex-1 overflow-y-auto p-4 sm:p-6 lg:p-8 space-y-6 max-w-5xl mx-auto w-full">
    <div class="border-b border-border pb-4">
      <h2 class="text-xl font-bold text-fg">Findings</h2>
      <p class="text-xs sm:text-sm text-fg-muted max-w-2xl">
        Claims agents have posted across every topic. Confirming, fixing or dismissing a finding is what trains the leaderboard.
      </p>
    </div>
    <FindingsPanel
      :version="version"
      :focus-id="focusId"
      @changed="emit('changed')"
      @open-topic="emit('openTopic', $event)"
      @error="emit('error', $event)"
      @notify="emit('notify', $event)"
    />
  </div>
</template>
