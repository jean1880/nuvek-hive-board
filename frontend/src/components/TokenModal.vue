<script setup lang="ts">
import { ref, watch } from 'vue'
import { Button, Modal } from '@nuvek/ui'
import { cancelTokenPrompt, submitToken, tokenPromptOpen } from '../lib/auth'

const token = ref('')
const remember = ref(false)

watch(tokenPromptOpen, open => {
  if (open) {
    token.value = ''
    remember.value = false
  }
})

function onSubmit() {
  if (!token.value.trim()) return
  submitToken(token.value, remember.value)
  token.value = ''
}
</script>

<template>
  <Modal :open="tokenPromptOpen" title="API token required" @close="cancelTokenPrompt">
    <form class="space-y-4 pt-1" @submit.prevent="onSubmit">
      <p class="text-xs text-fg-muted leading-relaxed">
        This board needs its API token. By default it is kept in this tab's session storage only,
        sent with each request, and forgotten when the tab closes or you choose "Forget token".
      </p>
      <div>
        <label for="hive-token" class="block text-2xs font-mono text-fg-muted mb-1">API token</label>
        <input
          id="hive-token"
          v-model="token"
          type="password"
          autocomplete="off"
          required
          class="w-full text-xs px-3 py-2 rounded-md bg-bg border border-border text-fg font-mono focus:outline-none focus:border-primary/50"
        />
      </div>
      <label class="flex items-start gap-2 text-xs text-fg-muted cursor-pointer">
        <input v-model="remember" type="checkbox" class="mt-0.5" />
        <span>
          Remember on this device
          <span class="block text-2xs text-fg-dim">
            Stores the token in this browser's local storage until you choose "Forget token". Leave off on a shared computer.
          </span>
        </span>
      </label>
      <div class="flex items-center justify-end gap-2 pt-2 border-t border-border">
        <Button variant="ghost" type="button" class="text-xs" @click="cancelTokenPrompt">Cancel</Button>
        <Button variant="primary" type="submit" class="text-xs" :disabled="!token.trim()">Use token</Button>
      </div>
    </form>
  </Modal>
</template>
