#pragma once

#define TESTMACRO

class Test {
  int m_int;
  double m_double;
public:
  static const char* name();
  Test(int foo);
  Test(double foo);

  static const int COUNTDOWN[];
  static const int* COUNTDOWN_PTR;
  static const int* countdown();
};

namespace testing {

typedef Test TypeAlias;

} // namespace testing

typedef testing::TypeAlias TypeAlias;

namespace bitfields {

struct First {
    unsigned char three_bits_byte_one : 3;
    // This starts a new byte, leaving 5 bits unused.
    unsigned char :0;

    unsigned char six_bits_byte_two : 6;
    unsigned char two_bits_byte_two : 2;

    /// Returns true if the bitfields match the arguments, false otherwise.
    bool assert(unsigned char first,
                unsigned char second,
                unsigned char third);
};

struct Second {
    int thirty_one_bits : 31;
    bool one_bit : 1;

    /// Returns true if the bitfields match the arguments, false otherwise.
    bool assert(int first,
                bool second);
};

enum ItemKind {
    ITEM_KIND_UNO,
    ITEM_KIND_DOS,
    ITEM_KIND_TRES,
};

struct Third {
    int flags : 28;
    bool is_whatever : 1;
    ItemKind kind : 3;

    /// Returns true if the bitfields match the arguments, false otherwise.
    bool assert(int first, bool second, ItemKind third);
};

} // namespace bitfields
