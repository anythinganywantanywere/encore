<template>
  <div class="page h-full flex flex-col overflow-hidden bg-surface">
    <div class="max-w-3xl mx-auto h-full flex flex-col w-full">
      <div class="flex-none p-5 pb-3">
        <div class="flex items-center gap-4 mb-2">
          <button @click="goBack" class="text-on-surface transition-colors">
            <ArrowLeftIcon class="w-6 h-6 cursor-pointer rtl:rotate-180" />
          </button>
        </div>
      </div>

      <div class="scrollbar-hidden pb-safe-nav flex-1 min-h-0 overflow-y-scroll px-5">
        <div class="space-y-6">
          <h1 class="text-4xl text-on-surface mt-12 mb-6">
            {{ $t('fas.title') }}
          </h1>

          <div class="bg-primary-container rounded-3xl p-5 -mx-1.5">
            <div class="flex items-center justify-between">
              <div class="flex items-center gap-3">
                <h2 class="text-base font-medium text-on-primary-container">
                  {{ $t('fas.toggle_title') }}
                </h2>
              </div>
              <ToggleSwitch v-model="isFasEnabled" @update:modelValue="toggleFas" />
            </div>
          </div>

          <InformationOutlineIcon class="text-on-surface-variant my-6" :size="22" />
          <p class="text-sm text-on-surface-variant leading-relaxed">
            {{ $t('fas.brief') }}
          </p>
        </div>
      </div>
    </div>
  </div>
</template>

<script setup>
import { ref, onMounted, onBeforeUnmount } from 'vue'
import { useRouter, onBeforeRouteLeave } from 'vue-router'
import { useEncoreConfigStore } from '@/stores/EncoreConfig'
import { createDebouncedSave } from '@/helpers/Debounce'

import ArrowLeftIcon from '@/components/icons/ArrowLeft.vue'
import ToggleSwitch from '@/components/ui/ToggleSwitch.vue'
import InformationOutlineIcon from '@/components/icons/InformationOutline.vue'

const router = useRouter()
const encoreConfigStore = useEncoreConfigStore()

const isFasEnabled = ref(false)

const debouncedSave = createDebouncedSave(() => encoreConfigStore.saveConfig(), 500)

onMounted(async () => {
  try {
    if (!encoreConfigStore.isLoaded) {
      await encoreConfigStore.loadConfig()
    }
    isFasEnabled.value = encoreConfigStore.isFasEnabled
  } catch (error) {
    console.error('Failed to load FAS setting:', error)
  }
})

onBeforeRouteLeave(async (to, from, next) => {
  try {
    await debouncedSave.flush()
    next()
  } catch (error) {
    console.error('Failed to save on route leave:', error)
    next(false)
  }
})

onBeforeUnmount(async () => {
  await debouncedSave.flush()
})

async function toggleFas(enabled) {
  isFasEnabled.value = enabled

  try {
    if (!encoreConfigStore.isLoaded) {
      await encoreConfigStore.loadConfig()
    }

    encoreConfigStore.setFas(enabled)
    debouncedSave.trigger()

    console.log(`FAS ${enabled ? 'enabled' : 'disabled'}`)
    // No reboot modal here, unlike disable_tweaks: the Rust
    // daemon watches config.json and attaches/detaches FAS
    // on the next loop iteration, so the toggle takes
    // effect without a reboot while the daemon runs. If
    // the daemon is stopped, it takes effect on next start.
  } catch (error) {
    console.error('Failed to set FAS:', error)
    isFasEnabled.value = encoreConfigStore.isFasEnabled
  }
}

function goBack() {
  router.back()
}
</script>
