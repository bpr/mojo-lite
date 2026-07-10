# expect: does not conform to trait 'Copyable'
@fieldwise_init
struct Handle:
    var id: Int

@fieldwise_init
struct Wrapper(Copyable):
    var h: Handle
