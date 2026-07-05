from std.collections import Dict


def main() raises:
    var joint_angles = Dict[String, Int]()
    joint_angles["joint_3"] = 128
    joint_angles["joint_8"] = 256
    # TODO: also record joint_8 at an angle of 256
    print("joint angle:", joint_angles["joint_3"])
    print("joints:", len(joint_angles))

