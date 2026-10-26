<script setup lang="ts">
import type { Message, ThreadNode } from '../types'
import MessageCard from './MessageCard.vue'

defineProps<{
  node: ThreadNode
  depth?: number
}>()

const emit = defineEmits<{
  (e: 'reply', message: Message): void
  (e: 'delete', message: Message): void
}>()
</script>

<template>
  <div class="relative space-y-3">
    <MessageCard
      :message="node.message"
      :is-reply="(depth || 0) > 0"
      @reply="emit('reply', $event)"
      @delete="emit('delete', $event)"
    />

    <div
      v-if="node.replies && node.replies.length > 0"
      class="space-y-3 relative before:absolute before:left-3 sm:before:left-5 before:top-0 before:bottom-3 before:w-0.5 before:bg-border/60"
    >
      <ThreadNodeView
        v-for="reply in node.replies"
        :key="reply.message.id"
        :node="reply"
        :depth="(depth || 0) + 1"
        @reply="emit('reply', $event)"
        @delete="emit('delete', $event)"
      />
    </div>
  </div>
</template>
