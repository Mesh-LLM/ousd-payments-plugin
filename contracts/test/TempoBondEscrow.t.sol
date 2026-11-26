// SPDX-License-Identifier: MIT OR Apache-2.0
pragma solidity 0.8.30;

import {TempoBondEscrow} from "../src/TempoBondEscrow.sol";

interface Vm {
    function etch(address target, bytes calldata code) external;
    function warp(uint256 timestamp) external;
    function prank(address sender) external;
    function expectRevert(bytes calldata reason) external;
}

contract MockOUSD {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;
    bool public rejectTransfers;
    bool public shortDeposit;

    function mint(address account, uint256 amount) external {
        balanceOf[account] += amount;
    }

    function setRejectTransfers(bool reject) external {
        rejectTransfers = reject;
    }

    function setShortDeposit(bool short) external {
        shortDeposit = short;
    }

    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        return true;
    }

    function transferFrom(address from, address to, uint256 amount) external returns (bool) {
        if (rejectTransfers) return false;
        allowance[from][msg.sender] -= amount;
        balanceOf[from] -= amount;
        balanceOf[to] += shortDeposit ? amount - 1 : amount;
        return true;
    }

    function transfer(address to, uint256 amount) external returns (bool) {
        if (rejectTransfers) return false;
        balanceOf[msg.sender] -= amount;
        balanceOf[to] += amount;
        return true;
    }
}

contract TempoBondEscrowTest {
    Vm private constant vm = Vm(address(uint160(uint256(keccak256("hevm cheat code")))));
    TempoBondEscrow private escrow;
    MockOUSD private ousd;
    bytes32 private constant MESH = bytes32(uint256(1));
    bytes32 private constant HOST = bytes32(uint256(2));
    uint96 private constant AMOUNT = 20_000_000;

    function setUp() public {
        escrow = new TempoBondEscrow();
        MockOUSD implementation = new MockOUSD();
        vm.etch(escrow.token(), address(implementation).code);
        ousd = MockOUSD(escrow.token());
        ousd.mint(address(this), 1_000_000_000);
        ousd.approve(address(escrow), type(uint256).max);
    }

    function testIndefiniteLockAndDelayedExitPreserveIdentityAndPrincipal() public {
        uint64 delay = 7 days;
        bytes32 id = escrow.lock(MESH, HOST, AMOUNT, delay, bytes32(uint256(10)));
        TempoBondEscrow.Bond memory bond = escrow.getBond(id);
        require(bond.meshId == MESH && bond.hostKey == HOST, "identity changed");
        require(bond.refundWallet == address(this) && bond.amount == AMOUNT, "refund claim changed");
        require(bond.exitDelay == delay && bond.withdrawAt == 0 && !bond.withdrawn, "lock changed");
        vm.expectRevert(bytes("bond already exists"));
        escrow.lock(MESH, bytes32(uint256(3)), AMOUNT, delay, bytes32(uint256(10)));
        escrow.lock(MESH, bytes32(uint256(3)), AMOUNT, delay, bytes32(uint256(11)));
        require(ousd.balanceOf(address(escrow)) == 2 * AMOUNT, "identities shared one deposit");
        vm.warp(block.timestamp + 365 days);
        vm.expectRevert(bytes("bond still locked or withdrawn"));
        escrow.withdraw(id);
        vm.prank(address(0x1234));
        vm.expectRevert(bytes("only funding wallet"));
        escrow.requestExit(id);
        escrow.requestExit(id);
        uint64 deadline = uint64(block.timestamp) + delay;
        require(escrow.getBond(id).withdrawAt == deadline, "exit delay did not start at request");
        vm.warp(deadline - 1);
        escrow.requestExit(id);
        require(escrow.getBond(id).withdrawAt == deadline, "repeated exit reset delay");
        vm.expectRevert(bytes("bond still locked or withdrawn"));
        escrow.withdraw(id);
        vm.warp(deadline);
        vm.prank(address(0x1234));
        vm.expectRevert(bytes("only funding wallet"));
        escrow.withdraw(id);
        uint256 beforeRefund = ousd.balanceOf(address(this));
        escrow.withdraw(id);
        require(ousd.balanceOf(address(this)) == beforeRefund + AMOUNT, "incomplete refund");
        require(escrow.getBond(id).withdrawn, "withdrawal not recorded");
        vm.expectRevert(bytes("bond still locked or withdrawn"));
        escrow.withdraw(id);
        vm.expectRevert(bytes("bond withdrawn"));
        escrow.requestExit(id);
        vm.expectRevert(bytes("bond already exists"));
        escrow.lock(MESH, HOST, AMOUNT, delay, bytes32(uint256(10)));
    }

    function testDepositAndRefundFailuresRemainRecoverable() public {
        bytes32 salt = bytes32(uint256(10));
        bytes32 id = escrow.bondId(address(this), salt);
        ousd.setRejectTransfers(true);
        vm.expectRevert(bytes("deposit failed"));
        escrow.lock(MESH, HOST, AMOUNT, 1 days, salt);
        require(escrow.getBond(id).refundWallet == address(0), "failed deposit left a bond");
        ousd.setRejectTransfers(false);
        escrow.lock(MESH, HOST, AMOUNT, 1 days, salt);
        escrow.requestExit(id);
        vm.warp(escrow.getBond(id).withdrawAt);
        ousd.setRejectTransfers(true);
        vm.expectRevert(bytes("refund failed"));
        escrow.withdraw(id);
        require(!escrow.getBond(id).withdrawn, "failed refund consumed claim");
        ousd.setRejectTransfers(false);
        escrow.withdraw(id);
        require(ousd.balanceOf(address(this)) == 1_000_000_000, "refund lost principal");
    }

    function testInvalidLocksCannotCreateEvidence() public {
        vm.expectRevert(bytes("empty identity"));
        escrow.lock(bytes32(0), HOST, AMOUNT, 1, bytes32(0));
        vm.expectRevert(bytes("empty identity"));
        escrow.lock(MESH, bytes32(0), AMOUNT, 1, bytes32(0));
        vm.expectRevert(bytes("invalid lock"));
        escrow.lock(MESH, HOST, 0, 1, bytes32(0));
        vm.expectRevert(bytes("invalid lock"));
        escrow.lock(MESH, HOST, AMOUNT, 0, bytes32(0));
    }

    function testInexactDepositCannotLeaveEvidenceOrLosePrincipal() public {
        bytes32 salt = bytes32(uint256(31));
        bytes32 id = escrow.bondId(address(this), salt);
        ousd.setShortDeposit(true);
        vm.expectRevert(bytes("inexact deposit"));
        escrow.lock(MESH, HOST, AMOUNT, 1 days, salt);
        require(escrow.getBond(id).refundWallet == address(0), "inexact deposit left evidence");
        require(ousd.balanceOf(address(this)) == 1_000_000_000, "failed lock consumed principal");
        require(ousd.balanceOf(address(escrow)) == 0, "failed lock funded escrow");
    }

    function testMaximumExitDelayCannotOverflowAndTrapPrincipal() public {
        vm.expectRevert(bytes("invalid lock"));
        escrow.lock(MESH, HOST, AMOUNT, type(uint64).max, bytes32(uint256(98)));
        bytes32 id = escrow.lock(MESH, HOST, AMOUNT, escrow.MAX_EXIT_DELAY(), bytes32(uint256(99)));
        uint256 expected = block.timestamp + uint256(escrow.MAX_EXIT_DELAY());
        escrow.requestExit(id);
        require(escrow.getBond(id).withdrawAt == expected, "exit timestamp truncated");
        vm.warp(expected);
        escrow.withdraw(id);
        require(ousd.balanceOf(address(this)) == 1_000_000_000, "large delay trapped principal");
    }

    function testFuzzRefundConservesPrincipal(uint96 proposedAmount, uint32 duration) public {
        uint96 amount = 1 + proposedAmount % 1_000_000_000;
        uint64 delay = 1 + uint64(duration) % 365 days;
        bytes32 id = escrow.lock(MESH, HOST, amount, delay, bytes32(uint256(20)));
        vm.warp(block.timestamp + 30 days);
        escrow.requestExit(id);
        vm.warp(escrow.getBond(id).withdrawAt);
        escrow.withdraw(id);
        require(ousd.balanceOf(address(this)) == 1_000_000_000, "principal changed");
        require(ousd.balanceOf(address(escrow)) == 0, "escrow retained principal");
    }
}
