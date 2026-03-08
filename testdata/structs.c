#include <stdio.h>
#include <string.h>

struct Address {
    char street[64];
    char city[32];
    int zip;
};

struct Person {
    char name[32];
    int age;
    struct Address address;
};

int main(void) {
    struct Person p;
    strncpy(p.name, "Alice", sizeof(p.name) - 1);
    p.name[sizeof(p.name) - 1] = '\0';
    p.age = 30;
    strncpy(p.address.street, "123 Main St", sizeof(p.address.street) - 1);
    p.address.street[sizeof(p.address.street) - 1] = '\0';
    strncpy(p.address.city, "Springfield", sizeof(p.address.city) - 1);
    p.address.city[sizeof(p.address.city) - 1] = '\0';
    p.address.zip = 62704;

    printf("%s is %d years old\n", p.name, p.age);
    printf("Lives at %s, %s %d\n", p.address.street, p.address.city, p.address.zip);
    return 0;
}
