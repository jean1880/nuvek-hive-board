<script setup lang="ts">
import {
  DropdownMenuContent,
  DropdownMenuItem,
  DropdownMenuPortal,
  DropdownMenuRoot,
  DropdownMenuTrigger,
} from 'reka-ui'

// A "more actions" menu for rarely used or destructive actions, kept away from the workflow buttons.
defineProps<{
  label: string
  items: { key: string; label: string; danger?: boolean }[]
}>()

const emit = defineEmits<{ (e: 'select', key: string): void }>()
</script>

<template>
  <DropdownMenuRoot>
    <DropdownMenuTrigger
      :aria-label="label"
      :title="label"
      class="text-xs px-2 py-1.5 rounded-md border border-border text-fg-muted hover:text-fg hover:bg-surface/50 cursor-pointer leading-none"
    >
      <span aria-hidden="true">⋯</span>
    </DropdownMenuTrigger>
    <DropdownMenuPortal>
      <DropdownMenuContent
        align="end"
        :side-offset="4"
        class="z-50 min-w-40 rounded-md border border-border-strong bg-surface p-1 shadow-lg"
      >
        <DropdownMenuItem
          v-for="item in items"
          :key="item.key"
          :class="[
            'text-xs px-2.5 py-1.5 rounded cursor-pointer outline-none data-[highlighted]:bg-surface-hover',
            item.danger ? 'text-error' : 'text-fg',
          ]"
          @select="emit('select', item.key)"
        >
          {{ item.label }}
        </DropdownMenuItem>
      </DropdownMenuContent>
    </DropdownMenuPortal>
  </DropdownMenuRoot>
</template>
