// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IERC20{
    function totalSupply() external view returns (uint256);
    function balanceOf(address account) external view returns (uint256);
    function transfer(address recipient, uint256 amount) external returns (bool);
    function allowance(address owner, address spender) external view returns (uint256);
    function approve(address spender, uint256 amount) external returns (bool);
    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool);
}


contract ERC20 is IERC20 {
    uint public totalSupply;
    mapping (address => uint) public balanceOF;
    mapping (address => mapping (address => uint)) public allowance;

    string public name = "Test";
    string public symbol = "Test";
    uint8 public decimals = 18;

    event Transfer(address indexed from, address indexed to, uint256 value);
    event Approval(address indexed owner, address indexed spender, uint256 value);

    function transfer(address recipient, uint256 amount) external returns (bool) {
        balanceOF[msg.sender] -= amount;
        balanceOF[recipient] += amount;
        emit Transfer(msg.sender, recipient, amount);
        return true;
    }
    function approve(address spender, uint256 amount) external returns (bool) {
        allowance[msg.sender][spender] = amount;
        emit Approval(msg.sender, spender, amount);
        return true;
    }

    function transferFrom(address sender, address recipient, uint256 amount) external returns (bool) {
        allowance[sender][msg.sender] -= amount;
        balanceOF[sender] -= amount;
        balanceOF[recipient] += amount;
        emit Transfer(sender, recipient, amount);
        return true;
    }

    function mint(uint account) external {
        balanceOF[msg.sender] += account;
        totalSupply += account;
        emit Transfer(address(0), msg.sender, account);
    }

    function burn(uint account) external {
        balanceOF[msg.sender] -= account;
        totalSupply -= account;
        emit Transfer(msg.sender, address(0), account);
    }


}
