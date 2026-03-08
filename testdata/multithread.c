#include <stdio.h>
#include <pthread.h>

void *worker(void *arg) {
    int id = *(int *)arg;
    printf("thread %d running\n", id);
    return NULL;
}

int main(void) {
    pthread_t t;
    int id = 1;
    pthread_create(&t, NULL, worker, &id);
    pthread_join(t, NULL);
    printf("thread joined\n");
    return 0;
}
