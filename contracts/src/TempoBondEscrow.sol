// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.30;

interface IOUSD {
    function balanceOf(address account) external view returns (uint256);
    function transferFrom(address from, address to, uint256 amount) external returns (bool);
    function transfer(address to, uint256 amount) external returns (bool);
}

/// A refundable capital lock for one mesh and one host key, without an administrator.
contract TempoBondEscrow {
    uint64 public constant MAX_EXIT_DELAY = type(uint32).max;
    address public constant token = 0x20c0000000000000000000006a37DA5C996874BE;

    struct Bond {
        bytes32 meshId;
        bytes32 hostKey;
        address refundWallet;
        uint96 amount;
        uint64 exitDelay;
        uint256 withdrawAt;
        bool withdrawn;
    }

    mapping(bytes32 => Bond) private records;
    bool private entered;

    event Locked(
        bytes32 indexed bondId,
        bytes32 indexed meshId,
        bytes32 indexed hostKey,
        address refundWallet,
        uint96 amount,
        uint64 exitDelay
    );
    event Withdrawn(bytes32 indexed bondId, address indexed refundWallet, uint96 amount);

    modifier nonReentrant() {
        require(!entered, "reentrant call");
        entered = true;
        _;
        entered = false;
    }

    function bondId(address fundingWallet, bytes32 salt) public view returns (bytes32) {
        return keccak256(abi.encode(block.chainid, address(this), fundingWallet, salt));
    }

    function getBond(bytes32 id) external view returns (Bond memory) {
        return records[id];
    }

    function lock(bytes32 meshId, bytes32 hostKey, uint96 amount, uint64 exitDelay, bytes32 salt)
        external
        nonReentrant
        returns (bytes32 id)
    {
        require(meshId != bytes32(0) && hostKey != bytes32(0), "empty identity");
        require(amount != 0 && exitDelay != 0 && exitDelay <= MAX_EXIT_DELAY, "invalid lock");
        id = bondId(msg.sender, salt);
        require(records[id].refundWallet == address(0), "bond already exists");
        records[id] = Bond(meshId, hostKey, msg.sender, amount, exitDelay, 0, false);
        uint256 beforeBalance = IOUSD(token).balanceOf(address(this));
        require(IOUSD(token).transferFrom(msg.sender, address(this), amount), "deposit failed");
        require(IOUSD(token).balanceOf(address(this)) == beforeBalance + amount, "inexact deposit");
        emit Locked(id, meshId, hostKey, msg.sender, amount, exitDelay);
    }

    event ExitRequested(bytes32 indexed bondId, uint256 withdrawAt);

    function requestExit(bytes32 id) external nonReentrant {
        Bond storage bond = records[id];
        require(bond.refundWallet == msg.sender, "only funding wallet");
        require(!bond.withdrawn, "bond withdrawn");
        if (bond.withdrawAt != 0) return;
        bond.withdrawAt = block.timestamp + uint256(bond.exitDelay);
        emit ExitRequested(id, bond.withdrawAt);
    }

    function withdraw(bytes32 id) external nonReentrant {
        Bond storage bond = records[id];
        require(bond.refundWallet == msg.sender, "only funding wallet");
        require(
            !bond.withdrawn && bond.withdrawAt != 0 && block.timestamp >= bond.withdrawAt,
            "bond still locked or withdrawn"
        );
        bond.withdrawn = true;
        require(IOUSD(token).transfer(bond.refundWallet, bond.amount), "refund failed");
        emit Withdrawn(id, bond.refundWallet, bond.amount);
    }
}
