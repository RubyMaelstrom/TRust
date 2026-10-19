// Valid independent Vulkan instances, with concurrent global discovery.
// https://docs.vulkan.org/spec/latest/chapters/fundamentals.html#fundamentals-threadingbehavior
// No browser, wgpu, FFmpeg or image resources participate in this probe.
#include <vulkan/vulkan.h>
#include <pthread.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

static pthread_barrier_t phase;
static pthread_mutex_t init_gate = PTHREAD_MUTEX_INITIALIZER;
static int serialized;
static unsigned rounds;
static int failed[2];

static VkResult enumerate_extensions(void) {
    uint32_t count = 0;
    VkResult result = vkEnumerateInstanceExtensionProperties(NULL, &count, NULL);
    if (result != VK_SUCCESS || count == 0) return result;
    if (count > 4096) return VK_ERROR_OUT_OF_HOST_MEMORY;
    VkExtensionProperties *properties = calloc(count, sizeof(*properties));
    if (properties == NULL) return VK_ERROR_OUT_OF_HOST_MEMORY;
    result = vkEnumerateInstanceExtensionProperties(NULL, &count, properties);
    free(properties);
    return result;
}

static void *worker(void *argument) {
    const unsigned id = (unsigned)(uintptr_t)argument;
    for (unsigned round = 0; round < rounds; ++round) {
        VkInstance instance = VK_NULL_HANDLE;
        VkApplicationInfo application = {
            .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO,
            .pApplicationName = "Vulkan initialization probe",
            .apiVersion = VK_API_VERSION_1_0,
        };
        VkInstanceCreateInfo create = {
            .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO,
            .pApplicationInfo = &application,
        };
        pthread_barrier_wait(&phase);
        if (serialized) pthread_mutex_lock(&init_gate);
        // A Vulkan client may query global extensions while another creates
        // its independent instance. This mirrors the captured native calls.
        VkResult result = id == 0 ? enumerate_extensions() : VK_SUCCESS;
        if (result == VK_SUCCESS || result == VK_INCOMPLETE)
            result = vkCreateInstance(&create, NULL, &instance);
        if (serialized) pthread_mutex_unlock(&init_gate);
        if (result != VK_SUCCESS) {
            fprintf(stderr, "thread=%u round=%u VkResult=%d\n", id, round, result);
            failed[id] = 1;
        }
        pthread_barrier_wait(&phase);
        if (result == VK_SUCCESS && instance != VK_NULL_HANDLE)
            vkDestroyInstance(instance, NULL);
        pthread_barrier_wait(&phase);
    }
    return NULL;
}

int main(int argc, char **argv) {
    if (argc != 3 || (strcmp(argv[1], "serial") && strcmp(argv[1], "parallel")))
        return 2;
    serialized = strcmp(argv[1], "serial") == 0;
    rounds = (unsigned)strtoul(argv[2], NULL, 10);
    if (rounds == 0 || rounds > 1000) return 2;
    printf("Vulkan headers=%u, mode=%s, independent instances=2, rounds=%u\n",
           VK_HEADER_VERSION, argv[1], rounds);
    fflush(stdout);
    if (pthread_barrier_init(&phase, NULL, 2) != 0) return 2;
    pthread_t threads[2];
    for (unsigned id = 0; id < 2; ++id)
        if (pthread_create(&threads[id], NULL, worker, (void *)(uintptr_t)id)) return 2;
    for (unsigned id = 0; id < 2; ++id) pthread_join(threads[id], NULL);
    pthread_barrier_destroy(&phase);
    printf("completed; errors=%d\n", failed[0] + failed[1]);
    return failed[0] || failed[1];
}
