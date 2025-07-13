#include <stdio.h>
#include <sys/syscall.h>
#include <unistd.h>

int main() {
    long result = syscall(2333);
    printf("Syscall 2333 returned: %ld\n", result);
    return 0;
}