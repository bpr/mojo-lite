# expect: does not conform to trait 'Writable'
@fieldwise_init
struct BrokenWritable(Writable):
    var value: Int

    def write_to(self, mut writer: String):
        writer = String(self.value)
