# Makefile — omega-uffd-bridge
# Compile bridge.c en shared library LD_PRELOAD.

CC      ?= gcc
CFLAGS  ?= -O2 -Wall -Wextra -fPIC
LDFLAGS ?= -shared -ldl -lpthread

TARGET  := omega-uffd-bridge.so
SRC     := bridge.c

.PHONY: all clean install

all: $(TARGET)

$(TARGET): $(SRC)
	$(CC) $(CFLAGS) $(LDFLAGS) -o $@ $<

install: $(TARGET)
	install -m 644 $(TARGET) /usr/local/lib/$(TARGET)
	ldconfig /usr/local/lib

clean:
	rm -f $(TARGET)
